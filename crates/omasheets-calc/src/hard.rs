//! Tick-backed volatiles, bounded spill, and dynamic OFFSET/INDIRECT.
use super::*;

#[derive(Clone, Debug)]
pub(super) struct SpillRecord {
    rows: usize,
    columns: usize,
    values: Vec<Value>,
    members: Vec<usize>,
}

fn fit_array(array: ArrayValue, rows: usize, columns: usize) -> ArrayValue {
    if array.rows == rows && array.columns == columns {
        return array;
    }
    let mut values = vec![Value::Error(CalcError::NotAvailable); rows.saturating_mul(columns)];
    for row in 0..rows.min(array.rows) {
        for column in 0..columns.min(array.columns) {
            values[row * columns + column] = array.at(row, column);
        }
    }
    ArrayValue {
        rows,
        columns,
        values,
    }
}

pub(crate) enum StepResult {
    Deferred,
    Barrier,
    Value(Value),
}

enum Class {
    Literal(Value),
    Barrier,
    Spill(usize),
    Formula(bool),
}

struct Binding {
    cells: Vec<CellId>,
    ranges: Vec<RangeKey>,
    present: bool,
}

enum SpillPreview {
    Scalar(Value),
    Array(ArrayValue),
}

/// Shifts `origin` by truncated `rows`/`cols` and sizes the rectangle.
/// Non-positive size and anything outside the grid is `#REF!`. A rectangle
/// over [`MAX_RANGE_CELLS`] is `#NUM!`.
pub(crate) fn rectangle_shift(
    origin: CellId,
    rows: f64,
    columns: f64,
    height: f64,
    width: f64,
) -> Result<(CellId, usize, usize), CalcError> {
    let rows = trunc_offset(rows)?;
    let columns = trunc_offset(columns)?;
    let height = trunc_offset(height)?;
    let width = trunc_offset(width)?;
    if height < 1 || width < 1 {
        return Err(CalcError::InvalidReference);
    }
    let rows_usize = height as usize;
    let columns_usize = width as usize;
    if rows_usize.saturating_mul(columns_usize) > MAX_RANGE_CELLS {
        return Err(CalcError::InvalidNumber);
    }
    let row = i64::from(origin.row)
        .checked_add(rows)
        .ok_or(CalcError::InvalidReference)?;
    let column = i64::from(origin.column)
        .checked_add(columns)
        .ok_or(CalcError::InvalidReference)?;
    if row < 0 || column < 0 {
        return Err(CalcError::InvalidReference);
    }
    let last_row = row
        .checked_add(height - 1)
        .ok_or(CalcError::InvalidReference)?;
    let last_column = column
        .checked_add(width - 1)
        .ok_or(CalcError::InvalidReference)?;
    if last_row >= i64::from(MAX_ROWS) || last_column >= i64::from(MAX_COLUMNS) {
        return Err(CalcError::InvalidReference);
    }
    Ok((
        CellId::new(origin.sheet, row as u32, column as u32),
        rows_usize,
        columns_usize,
    ))
}

fn trunc_offset(value: f64) -> Result<i64, CalcError> {
    if !value.is_finite() || value.abs() > 1.0e9 {
        return Err(CalcError::InvalidNumber);
    }
    Ok(value.trunc() as i64)
}

/// Resolves one A1 reference or rectangle, with an optional sheet name.
/// R1C1, 3D references, external workbooks and anything else are `None`.
pub(crate) fn resolve_a1_reference(
    text: &str,
    origin: u32,
    sheet_names: &HashMap<String, u32>,
) -> Option<Expr<CellId>> {
    let text = text.trim();
    if text.is_empty() || text.contains('[') || text.contains('(') || text.contains('#') {
        return None;
    }
    let (sheet, body) = if let Some(rest) = text.strip_prefix('\'') {
        let (name, rest) = rest.split_once('\'')?;
        let body = rest.trim_start().strip_prefix('!')?.trim();
        if name.contains(':') {
            return None;
        }
        let sheet = *sheet_names.get(&name.replace("''", "'").to_lowercase())?;
        (sheet, body)
    } else if let Some((name, body)) = text.split_once('!') {
        if name.contains(':') {
            return None;
        }
        let sheet = *sheet_names.get(&name.trim().to_lowercase())?;
        (sheet, body.trim())
    } else {
        (origin, text)
    };
    if body.is_empty() || body.contains('!') {
        return None;
    }
    if let Some((left, right)) = body.split_once(':') {
        if right.contains(':') {
            return None;
        }
        let left = left.trim();
        let right = right.trim();
        if let (Ok(first), Ok(second)) = (parse_a1(left, sheet), parse_a1(right, sheet)) {
            return expand_range(first, second).ok();
        }
        if let (Some(first), Some(second)) =
            (super::column_number(left), super::column_number(right))
        {
            let start = first.min(second);
            let end = first.max(second);
            return expand_range(
                CellId::new(sheet, 0, start),
                CellId::new(sheet, super::MAX_ROWS - 1, end),
            )
            .ok();
        }
        if let (Some(first), Some(second)) = (super::row_number(left), super::row_number(right)) {
            return expand_range(
                CellId::new(sheet, first.min(second), 0),
                CellId::new(sheet, first.max(second), super::MAX_COLUMNS - 1),
            )
            .ok();
        }
        return None;
    }
    parse_a1(body, sheet).ok().map(Expr::Reference)
}

pub(crate) fn expression_is_volatile(expression: &Expr<CellId>) -> bool {
    match expression {
        Expr::Function(
            Function::Today | Function::Now | Function::Rand | Function::RandBetween,
            _,
        ) => true,
        Expr::Function(_, arguments) => arguments.iter().any(expression_is_volatile),
        Expr::UnaryMinus(inner) | Expr::Percent(inner) => expression_is_volatile(inner),
        Expr::Binary(_, left, right) => {
            expression_is_volatile(left) || expression_is_volatile(right)
        }
        _ => false,
    }
}

fn formula_needs_dynamic(expression: &Expr<usize>) -> bool {
    match expression {
        Expr::Function(Function::Offset | Function::Indirect, _) => true,
        Expr::Function(_, arguments) => arguments.iter().any(formula_needs_dynamic),
        Expr::UnaryMinus(inner) | Expr::Percent(inner) => formula_needs_dynamic(inner),
        Expr::Binary(_, left, right) => formula_needs_dynamic(left) || formula_needs_dynamic(right),
        _ => false,
    }
}

/// Uniform draw in `[0, 1)`, the range Microsoft documents for `RAND`.
/// Excel does not publish the seed of a saved workbook, so a new tick uses
/// this per-cell value. The same tick and cell replay it.
fn volatile_unit(tick: u64, cell: CellId) -> f64 {
    let mut z = tick.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ u64::from(cell.row).wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ u64::from(cell.column).wrapping_mul(0x94D0_49BB_1331_11EB)
        ^ u64::from(cell.sheet).wrapping_mul(0xD1B5_4A32_D192_E40F);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z ^= tick.rotate_left(17);
    ((z >> 11) as f64) / ((1_u64 << 53) as f64)
}

/// `RANDBETWEEN`: both bounds truncate toward zero, then the result is an
/// integer from `bottom` through `top`. `bottom > top` is `#NUM!`.
fn rand_between(unit: f64, bottom: f64, top: f64) -> Result<f64, CalcError> {
    let bottom = trunc_offset(bottom)? as f64;
    let top = trunc_offset(top)? as f64;
    if bottom > top {
        return Err(CalcError::InvalidNumber);
    }
    let span = top - bottom + 1.0;
    if !span.is_finite() || span <= 0.0 {
        return Err(CalcError::InvalidNumber);
    }
    let picked = bottom + (unit * span).floor();
    if !picked.is_finite() {
        return Err(CalcError::InvalidNumber);
    }
    Ok(picked.min(top))
}

fn excel_integer(value: f64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let rounded = value.round();
    if (value - rounded).abs() > 1e-9 || rounded.abs() > i64::MAX as f64 {
        return None;
    }
    Some(rounded as i64)
}

impl Workbook {
    /// Records the next tick at Unix millisecond `at` and recalculates every
    /// formula that reads the tick. Does not read the system clock.
    pub fn set_tick(&mut self, at: i64) -> RecalcReport {
        let tick = self
            .tick
            .map(|(tick, _)| tick.saturating_add(1))
            .unwrap_or(1);
        self.install_tick(tick, at)
    }

    /// Installs a known tick, such as the pair already stored on a snapshot.
    pub fn install_tick(&mut self, tick: u64, at: i64) -> RecalcReport {
        self.rand_replay.clear();
        self.tick = Some((tick, at));
        self.touch_tick()
    }

    fn touch_tick(&mut self) -> RecalcReport {
        let Some(node) = self.tick_node else {
            return RecalcReport::default();
        };
        if let Some(pending) = &mut self.bulk {
            pending.push(node);
            return RecalcReport::default();
        }
        self.recalculate(&[node])
    }

    pub(crate) fn ensure_tick_node(&mut self) -> usize {
        if let Some(node) = self.tick_node {
            return node;
        }
        let node = self.cells.len();
        self.cells.push(Cell {
            id: CellId::new(u32::MAX, 0, 0),
            input: Input::Tick,
            dependencies: Vec::new(),
            dependents: Vec::new(),
            value: Value::Blank,
        });
        self.dirty_marks.push(0);
        self.pending.push(0);
        self.eval_marks.push(0);
        self.tick_node = Some(node);
        node
    }

    pub(crate) fn evaluate_volatile(&self, function: Function, arguments: &[Expr<usize>]) -> Value {
        let arity = match function {
            Function::RandBetween => 2,
            Function::Today | Function::Now | Function::Rand => 0,
            _ => return Value::Error(CalcError::InvalidArguments),
        };
        if arguments.len() != arity {
            return Value::Error(CalcError::InvalidArguments);
        }
        if function == Function::RandBetween {
            let bottom = match excel_number(self.evaluate(&arguments[0])) {
                Ok(value) => value,
                Err(error) => return Value::Error(error),
            };
            let top = match excel_number(self.evaluate(&arguments[1])) {
                Ok(value) => value,
                Err(error) => return Value::Error(error),
            };
            let bottom_i = match trunc_offset(bottom) {
                Ok(value) => value,
                Err(error) => return Value::Error(error),
            };
            let top_i = match trunc_offset(top) {
                Ok(value) => value,
                Err(error) => return Value::Error(error),
            };
            if bottom_i > top_i {
                return Value::Error(CalcError::InvalidNumber);
            }
            let cell = self.evaluating.get();
            if let Some(&cached) = self.rand_replay.get(&cell) {
                if let Some(integer) = excel_integer(cached) {
                    if (bottom_i..=top_i).contains(&integer) {
                        return Value::Number(integer as f64);
                    }
                }
            }
            let Some((tick, _)) = self.tick else {
                return Value::Error(CalcError::NotAvailable);
            };
            return match rand_between(volatile_unit(tick, cell), bottom, top) {
                Ok(value) => number_value(value),
                Err(error) => Value::Error(error),
            };
        }
        if function == Function::Rand {
            let cell = self.evaluating.get();
            if let Some(&cached) = self.rand_replay.get(&cell) {
                if (0.0..1.0).contains(&cached) {
                    return Value::Number(cached);
                }
            }
        }
        let Some((tick, at)) = self.tick else {
            return Value::Error(CalcError::NotAvailable);
        };
        match function {
            Function::Today => {
                match serial_date::serial_from_unix_millis_in(self.date_system, at) {
                    Ok(serial) => Value::Number(serial.trunc()),
                    Err(error) => Value::Error(error),
                }
            }
            Function::Now => match serial_date::serial_from_unix_millis_in(self.date_system, at) {
                Ok(serial) => number_value(serial),
                Err(error) => Value::Error(error),
            },
            Function::Rand => Value::Number(volatile_unit(tick, self.evaluating.get())),
            _ => Value::Error(CalcError::InvalidArguments),
        }
    }

    pub(crate) fn dynamic_error_for_current(&self) -> bool {
        let id = self.evaluating.get();
        self.indices
            .get(&id)
            .is_some_and(|index| self.dynamic_errors.contains_key(index))
    }

    pub(crate) fn preview_dynamic_binding(
        &self,
        origin: u32,
        expression: &Expr<CellId>,
    ) -> (Vec<CellId>, Vec<RangeKey>) {
        let mut binding = Binding {
            cells: Vec::new(),
            ranges: Vec::new(),
            present: false,
        };
        self.walk_parsed(origin, expression, &mut binding);
        (binding.cells, binding.ranges)
    }

    fn walk_parsed(&self, origin: u32, expression: &Expr<CellId>, binding: &mut Binding) {
        match expression {
            Expr::Function(Function::Offset, arguments) => {
                binding.present = true;
                self.note_parsed_offset(arguments, binding);
                for argument in arguments {
                    self.walk_parsed(origin, argument, binding);
                }
            }
            Expr::Function(Function::ReferenceSpan, arguments) => {
                binding.present = true;
                self.note_parsed_span(arguments, binding);
                for argument in arguments.iter().take(2) {
                    self.walk_parsed(origin, argument, binding);
                }
            }
            Expr::Function(Function::Indirect, arguments) => {
                binding.present = true;
                self.note_parsed_indirect(origin, arguments, binding);
                for argument in arguments {
                    self.walk_parsed(origin, argument, binding);
                }
            }
            Expr::Function(_, arguments) => {
                for argument in arguments {
                    self.walk_parsed(origin, argument, binding);
                }
            }
            Expr::UnaryMinus(inner) | Expr::Percent(inner) => {
                self.walk_parsed(origin, inner, binding)
            }
            Expr::Binary(_, left, right) => {
                self.walk_parsed(origin, left, binding);
                self.walk_parsed(origin, right, binding);
            }
            _ => {}
        }
    }

    fn note_parsed_span(&self, arguments: &[Expr<CellId>], binding: &mut Binding) {
        let [first, last, _] = arguments else {
            return;
        };
        let Some((first_start, first_end)) = self.parsed_corners(first) else {
            return;
        };
        let Some((last_start, last_end)) = self.parsed_corners(last) else {
            return;
        };
        let corners = [first_start, first_end, last_start, last_end];
        let sheet = corners[0].sheet;
        if corners.iter().any(|cell| cell.sheet != sheet) {
            return;
        }
        let min_row = corners.iter().map(|cell| cell.row).min().unwrap();
        let max_row = corners.iter().map(|cell| cell.row).max().unwrap();
        let min_column = corners.iter().map(|cell| cell.column).min().unwrap();
        let max_column = corners.iter().map(|cell| cell.column).max().unwrap();
        binding.ranges.push(range_key(
            CellId::new(sheet, min_row, min_column),
            None,
            (max_row - min_row + 1) as usize,
            (max_column - min_column + 1) as usize,
        ));
    }

    fn parsed_corners(&self, expression: &Expr<CellId>) -> Option<(CellId, CellId)> {
        match expression {
            Expr::Reference(cell) => Some((*cell, *cell)),
            Expr::Range {
                anchor,
                rows,
                columns,
                members: None,
            } => Some((
                *anchor,
                CellId::new(
                    anchor.sheet,
                    anchor.row + *rows as u32 - 1,
                    anchor.column + *columns as u32 - 1,
                ),
            )),
            Expr::Function(Function::Offset, arguments) => {
                let (origin, end) = reference_bounds(arguments.first()?)?;
                let rows = self.parsed_number(arguments.get(1)?)?;
                let columns = self.parsed_number(arguments.get(2)?)?;
                let base_rows = f64::from(end.row - origin.row + 1);
                let base_columns = f64::from(end.column - origin.column + 1);
                let height = match arguments.get(3) {
                    None | Some(Expr::Empty) => base_rows,
                    Some(expression) => self.parsed_number(expression)?,
                };
                let width = match arguments.get(4) {
                    None | Some(Expr::Empty) => base_columns,
                    Some(expression) => self.parsed_number(expression)?,
                };
                let (anchor, rows, columns) =
                    rectangle_shift(origin, rows, columns, height, width).ok()?;
                Some((
                    anchor,
                    CellId::new(
                        anchor.sheet,
                        anchor.row + rows as u32 - 1,
                        anchor.column + columns as u32 - 1,
                    ),
                ))
            }
            _ => None,
        }
    }

    fn note_compiled_span(&self, arguments: &[Expr<usize>], binding: &mut Binding) {
        let Ok(view) = self.reference_span(arguments) else {
            return;
        };
        binding
            .ranges
            .push(range_key(view.anchor, None, view.rows, view.columns));
    }

    fn note_parsed_offset(&self, arguments: &[Expr<CellId>], binding: &mut Binding) {
        if !(3..=5).contains(&arguments.len()) {
            return;
        }
        if !matches!(
            arguments[0],
            Expr::Reference(_) | Expr::Range { members: None, .. }
        ) {
            return;
        }
        let Some((anchor, end)) = reference_bounds(&arguments[0]) else {
            return;
        };
        let Some(rows) = self.parsed_number(&arguments[1]) else {
            return;
        };
        let Some(columns) = self.parsed_number(&arguments[2]) else {
            return;
        };
        let height = match arguments.get(3) {
            None | Some(Expr::Empty) => f64::from(end.row - anchor.row + 1),
            Some(expression) => match self.parsed_number(expression) {
                Some(value) => value,
                None => return,
            },
        };
        let width = match arguments.get(4) {
            None | Some(Expr::Empty) => f64::from(end.column - anchor.column + 1),
            Some(expression) => match self.parsed_number(expression) {
                Some(value) => value,
                None => return,
            },
        };
        if let Ok((origin, rows, columns)) = rectangle_shift(anchor, rows, columns, height, width) {
            binding.ranges.push(range_key(origin, None, rows, columns));
        }
    }

    fn note_parsed_indirect(&self, origin: u32, arguments: &[Expr<CellId>], binding: &mut Binding) {
        let [Expr::Reference(cell)] = arguments else {
            return;
        };
        let Value::Text(text) = self.value(*cell) else {
            return;
        };
        if let Some(resolved) = resolve_a1_reference(&text, origin, &self.sheet_names) {
            push_resolved(resolved, binding);
        }
    }

    fn parsed_number(&self, expression: &Expr<CellId>) -> Option<f64> {
        match expression {
            Expr::Number(value) if value.is_finite() => Some(*value),
            Expr::Reference(cell) => {
                let index = self.indices.get(cell)?;
                let stored = &self.cells[*index];
                // A bulk load keeps `value` blank until the final pass, while
                // the literal already sits on the cell. A formula that has not
                // been calculated yet is unknown, not zero: treating it as
                // zero makes `OFFSET(N41,D41,0)` look like a self-reference.
                let value = match (&stored.value, &stored.input) {
                    (Value::Blank, Input::Literal(literal)) => literal,
                    (Value::Blank, Input::Formula(_)) => return None,
                    (value, _) => value,
                };
                match value {
                    Value::Number(value) if value.is_finite() => Some(*value),
                    Value::Blank => Some(0.0),
                    Value::Boolean(value) => Some(if *value { 1.0 } else { 0.0 }),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub(crate) fn step_cell(&mut self, index: usize, generation: u64) -> StepResult {
        let class = match &self.cells[index].input {
            Input::Literal(value) => Class::Literal(value.clone()),
            Input::Range { .. } | Input::Tick => Class::Barrier,
            Input::Vacant => unreachable!("retired ranges have no edges"),
            Input::Spill { anchor } => Class::Spill(*anchor),
            Input::Formula(expression) => Class::Formula(formula_needs_dynamic(expression)),
        };
        match class {
            Class::Literal(value) => StepResult::Value(value),
            Class::Barrier => StepResult::Barrier,
            Class::Spill(anchor) => StepResult::Value(self.spill_member_value(index, anchor)),
            Class::Formula(dynamic) => {
                self.evaluating.set(self.cells[index].id);
                if dynamic && self.sync_dynamic(index, generation) {
                    StepResult::Deferred
                } else {
                    StepResult::Value(self.evaluate_formula_cell(index))
                }
            }
        }
    }

    fn sync_dynamic(&mut self, formula: usize, generation: u64) -> bool {
        let expression = match &self.cells[formula].input {
            Input::Formula(expression) => expression.clone(),
            _ => return false,
        };
        let binding = self.binding_of_compiled(&expression);
        if !binding.present {
            self.dynamic_errors.remove(&formula);
            let _ = self.apply_dynamic(formula, Vec::new());
            return false;
        }
        let mut indices = Vec::new();
        for cell in binding.cells {
            indices.push(self.ensure_cell(cell));
        }
        for key in binding.ranges {
            indices.push(self.ensure_range(&key));
        }
        indices.sort_unstable();
        indices.dedup();
        if self.apply_dynamic(formula, indices.clone()).is_err() {
            self.dynamic_errors
                .insert(formula, CalcError::InvalidReference);
            let _ = self.apply_dynamic(formula, Vec::new());
            return false;
        }
        self.dynamic_errors.remove(&formula);
        // A rectangle discovered during this pass was not in the opening
        // dirty set, so nothing is waiting on it yet. Arm it from the cells
        // it covers, or the formula reads them while they are still blank.
        self.arm_late_ranges(&indices, generation);
        let waiting = indices
            .iter()
            .copied()
            .filter(|dependency| {
                self.dirty_marks.get(*dependency).copied() == Some(generation)
                    && self.eval_marks.get(*dependency).copied() != Some(generation)
                    && !matches!(self.cells[*dependency].input, Input::Tick)
            })
            .count();
        if waiting == 0 {
            return false;
        }
        self.pending[formula] = waiting;
        true
    }

    /// Puts a range node discovered mid-pass onto the same barrier the
    /// opening mark would have built. Cells already calculated are not
    /// counted; each cell still to run decrements the barrier once.
    fn arm_late_ranges(&mut self, indices: &[usize], generation: u64) {
        for node in indices {
            if !matches!(self.cells[*node].input, Input::Range { .. }) {
                continue;
            }
            if self.eval_marks.get(*node).copied() == Some(generation)
                || self.dirty_marks.get(*node).copied() == Some(generation)
            {
                continue;
            }
            let waiting = self.unevaluated_covered(*node, generation);
            if waiting == 0 {
                self.eval_marks[*node] = generation;
                continue;
            }
            self.dirty_marks[*node] = generation;
            self.pending[*node] = waiting;
            self.late_range_barriers += 1;
        }
    }

    fn unevaluated_covered(&self, node: usize, generation: u64) -> usize {
        let still_due = |index: usize| {
            self.dirty_marks.get(index).copied() == Some(generation)
                && self.eval_marks.get(index).copied() != Some(generation)
        };
        match self.cells[node].input {
            Input::Range {
                shape:
                    RangeShape::Rectangle {
                        anchor,
                        rows,
                        columns,
                    },
            } => {
                let mut count = 0;
                self.for_each_rectangle_cell(anchor, rows, columns, |_position, index| {
                    if still_due(index) {
                        count += 1;
                    }
                });
                count
            }
            Input::Range {
                shape: RangeShape::Members { .. },
            } => self.cells[node]
                .dependencies
                .iter()
                .filter(|index| still_due(**index))
                .count(),
            Input::Range {
                shape:
                    RangeShape::Stack {
                        first_sheet,
                        last_sheet,
                        row,
                        column,
                        rows,
                        columns,
                    },
            } => {
                let mut count = 0;
                self.for_each_stack_cell(
                    first_sheet,
                    last_sheet,
                    row,
                    column,
                    rows,
                    columns,
                    |_position, index| {
                        if still_due(index) {
                            count += 1;
                        }
                    },
                );
                count
            }
            _ => 0,
        }
    }

    fn binding_of_compiled(&self, expression: &Expr<usize>) -> Binding {
        let mut binding = Binding {
            cells: Vec::new(),
            ranges: Vec::new(),
            present: false,
        };
        self.walk_compiled(expression, &mut binding);
        binding
    }

    fn walk_compiled(&self, expression: &Expr<usize>, binding: &mut Binding) {
        match expression {
            Expr::Function(Function::Offset, arguments) => {
                binding.present = true;
                self.note_offset(arguments, binding);
                for argument in arguments {
                    self.walk_compiled(argument, binding);
                }
            }
            Expr::Function(Function::ReferenceSpan, arguments) => {
                binding.present = true;
                self.note_compiled_span(arguments, binding);
                for argument in arguments.iter().take(2) {
                    self.walk_compiled(argument, binding);
                }
            }
            Expr::Function(Function::Indirect, arguments) => {
                binding.present = true;
                self.note_indirect(arguments, binding);
                for argument in arguments {
                    self.walk_compiled(argument, binding);
                }
            }
            Expr::Function(_, arguments) => {
                for argument in arguments {
                    self.walk_compiled(argument, binding);
                }
            }
            Expr::UnaryMinus(inner) | Expr::Percent(inner) => self.walk_compiled(inner, binding),
            Expr::Binary(_, left, right) => {
                self.walk_compiled(left, binding);
                self.walk_compiled(right, binding);
            }
            _ => {}
        }
    }

    fn note_offset(&self, arguments: &[Expr<usize>], binding: &mut Binding) {
        if !(3..=5).contains(&arguments.len()) {
            return;
        }
        let Ok(view) = self.reference_view(&arguments[0]) else {
            return;
        };
        let (origin, base_rows, base_columns) = view.origin(self);
        let Ok(rows) = number(self.evaluate(&arguments[1])) else {
            return;
        };
        let Ok(columns) = number(self.evaluate(&arguments[2])) else {
            return;
        };
        let height = match arguments.get(3) {
            None | Some(Expr::Empty) => base_rows as f64,
            Some(expression) => match number(self.evaluate(expression)) {
                Ok(value) => value,
                Err(_) => return,
            },
        };
        let width = match arguments.get(4) {
            None | Some(Expr::Empty) => base_columns as f64,
            Some(expression) => match number(self.evaluate(expression)) {
                Ok(value) => value,
                Err(_) => return,
            },
        };
        if let Ok((anchor, rows, columns)) = rectangle_shift(origin, rows, columns, height, width) {
            binding.ranges.push(RangeKey::Rectangle {
                anchor,
                rows,
                columns,
            });
        }
    }

    fn note_indirect(&self, arguments: &[Expr<usize>], binding: &mut Binding) {
        if arguments.len() != 1 {
            return;
        }
        let Value::Text(text) = self.evaluate(&arguments[0]) else {
            return;
        };
        let origin = self.evaluating.get().sheet;
        if let Some(resolved) = resolve_a1_reference(&text, origin, &self.sheet_names) {
            push_resolved(resolved, binding);
        }
    }

    fn apply_dynamic(&mut self, formula: usize, dynamic: Vec<usize>) -> Result<(), ()> {
        let old = self
            .dynamic_edges
            .get(&formula)
            .cloned()
            .unwrap_or_default();
        let structural: HashSet<usize> = self.cells[formula]
            .dependencies
            .iter()
            .copied()
            .filter(|dependency| !old.contains(dependency))
            .collect();
        let mut dynamic: Vec<usize> = dynamic
            .into_iter()
            .filter(|dependency| !structural.contains(dependency))
            .collect();
        dynamic.sort_unstable();
        dynamic.dedup();
        if dynamic == old {
            return Ok(());
        }
        let mut all: Vec<usize> = structural
            .iter()
            .copied()
            .chain(dynamic.iter().copied())
            .collect();
        all.sort_unstable();
        all.dedup();
        let cell = self.cells[formula].id;
        let cells: BTreeSet<CellId> = all
            .iter()
            .filter_map(|index| match self.cells[*index].input {
                Input::Range { .. } | Input::Tick => None,
                _ => Some(self.cells[*index].id),
            })
            .collect();
        let ranges = all
            .iter()
            .copied()
            .filter(|index| matches!(self.cells[*index].input, Input::Range { .. }));
        if self.prospective_cycle(cell, &cells, ranges).is_some() {
            return Err(());
        }
        for dependency in &old {
            self.cells[*dependency]
                .dependents
                .retain(|dependent| *dependent != formula);
            self.cells[formula]
                .dependencies
                .retain(|candidate| candidate != dependency);
            self.retire_range(*dependency);
        }
        for dependency in &dynamic {
            if !self.cells[formula].dependencies.contains(dependency) {
                self.cells[formula].dependencies.push(*dependency);
            }
            if !self.cells[*dependency].dependents.contains(&formula) {
                self.cells[*dependency].dependents.push(formula);
            }
        }
        if dynamic.is_empty() {
            self.dynamic_edges.remove(&formula);
        } else {
            self.dynamic_edges.insert(formula, dynamic);
        }
        Ok(())
    }

    fn evaluate_formula_cell(&mut self, index: usize) -> Value {
        let expression = match &self.cells[index].input {
            Input::Formula(expression) => expression.clone(),
            _ => return Value::Blank,
        };
        if let Some(&(rows, columns)) = self.array_formulas.get(&self.cells[index].id) {
            let array = match self.evaluate_array(&expression) {
                Ok(array) => fit_array(array, rows, columns),
                Err(error) => {
                    return Value::Error(error);
                }
            };
            self.clear_array_literals(index, array.rows, array.columns);
            return self.publish_spill(index, array);
        }
        let preview = match &expression {
            Expr::Array(array) if array.rows.saturating_mul(array.columns) > 1 => {
                SpillPreview::Array(array.clone())
            }
            Expr::Function(
                function @ (Function::Transpose
                | Function::MMult
                | Function::Filter
                | Function::Unique
                | Function::Sort
                | Function::Linest),
                arguments,
            ) => match self.array_result(*function, arguments) {
                Ok(array) if array.rows.saturating_mul(array.columns) > 1 => {
                    SpillPreview::Array(array)
                }
                Ok(array) => {
                    SpillPreview::Scalar(array.values.into_iter().next().unwrap_or(Value::Blank))
                }
                Err(error) => SpillPreview::Scalar(Value::Error(error)),
            },
            other => SpillPreview::Scalar(self.evaluate_stored(other)),
        };
        match preview {
            SpillPreview::Scalar(value) => {
                if self.spills.contains_key(&index) {
                    let cleared = self.retract_spill(index);
                    self.spill_followups.extend(cleared);
                }
                value
            }
            SpillPreview::Array(array) => self.publish_spill(index, array),
        }
    }

    /// Writes a root array into the rectangle under the formula. A blocked or
    /// out-of-grid rectangle is `#SPILL!`. More than [`MAX_RANGE_CELLS`] values
    /// is `#NUM!` and is not written.
    fn publish_spill(&mut self, anchor: usize, array: ArrayValue) -> Value {
        let id = self.cells[anchor].id;
        let count = array.rows.saturating_mul(array.columns);
        if array.rows == 0
            || array.columns == 0
            || array.values.len() != count
            || count > MAX_RANGE_CELLS
        {
            let cleared = self.retract_spill(anchor);
            self.spill_followups.extend(cleared);
            return Value::Error(CalcError::InvalidNumber);
        }
        let last_row = id.row as u64 + array.rows as u64 - 1;
        let last_column = id.column as u64 + array.columns as u64 - 1;
        if last_row >= u64::from(MAX_ROWS) || last_column >= u64::from(MAX_COLUMNS) {
            let cleared = self.retract_spill(anchor);
            self.spill_followups.extend(cleared);
            return Value::Error(CalcError::Spill);
        }
        let blocked = (0..array.rows).any(|row| {
            (0..array.columns).any(|column| {
                if row == 0 && column == 0 {
                    return false;
                }
                let cell = CellId::new(id.sheet, id.row + row as u32, id.column + column as u32);
                self.indices
                    .get(&cell)
                    .is_some_and(|index| self.blocks_spill(*index, anchor))
            })
        });
        let old_members = self
            .spills
            .remove(&anchor)
            .map(|spill| spill.members)
            .unwrap_or_default();
        let cleared = self.clear_members(&old_members, anchor);
        if blocked {
            self.spills.insert(
                anchor,
                SpillRecord {
                    rows: array.rows,
                    columns: array.columns,
                    values: Vec::new(),
                    members: Vec::new(),
                },
            );
            self.spill_followups.extend(cleared);
            return Value::Error(CalcError::Spill);
        }
        let mut members = Vec::new();
        for row in 0..array.rows {
            for column in 0..array.columns {
                if row == 0 && column == 0 {
                    continue;
                }
                let cell = CellId::new(id.sheet, id.row + row as u32, id.column + column as u32);
                let member = self.ensure_cell(cell);
                self.cells[member].input = Input::Spill { anchor };
                self.cells[member].value = array.values[row * array.columns + column].clone();
                self.cells[member].dependencies.clear();
                members.push(member);
            }
        }
        let anchor_value = array.values[0].clone();
        self.spills.insert(
            anchor,
            SpillRecord {
                rows: array.rows,
                columns: array.columns,
                values: array.values,
                members: members.clone(),
            },
        );
        self.spill_followups.extend(cleared);
        self.spill_followups.extend(members);
        anchor_value
    }

    fn clear_array_literals(&mut self, anchor: usize, rows: usize, columns: usize) {
        let id = self.cells[anchor].id;
        for row in 0..rows {
            for column in 0..columns {
                if row == 0 && column == 0 {
                    continue;
                }
                let cell = CellId::new(id.sheet, id.row + row as u32, id.column + column as u32);
                let Some(index) = self.indices.get(&cell).copied() else {
                    continue;
                };
                if matches!(self.cells[index].input, Input::Literal(_) | Input::Vacant) {
                    self.cells[index].input = Input::Vacant;
                    self.cells[index].value = Value::Blank;
                    self.cells[index].dependencies.clear();
                }
            }
        }
    }

    fn blocks_spill(&self, index: usize, anchor: usize) -> bool {
        match &self.cells[index].input {
            Input::Spill { anchor: owner } if *owner == anchor => false,
            Input::Literal(Value::Blank) | Input::Vacant => false,
            _ => true,
        }
    }

    pub(crate) fn retract_spill(&mut self, anchor: usize) -> Vec<usize> {
        let Some(spill) = self.spills.remove(&anchor) else {
            return Vec::new();
        };
        self.clear_members(&spill.members, anchor)
    }

    fn clear_members(&mut self, members: &[usize], anchor: usize) -> Vec<usize> {
        let mut cleared = Vec::new();
        for member in members {
            if matches!(self.cells[*member].input, Input::Spill { anchor: owner } if owner == anchor)
            {
                self.cells[*member].input = Input::Literal(Value::Blank);
                self.cells[*member].value = Value::Blank;
                self.cells[*member].dependencies.clear();
                cleared.push(*member);
            }
        }
        cleared
    }

    pub(crate) fn anchors_covering(&self, cell: CellId) -> Vec<usize> {
        self.spills
            .iter()
            .filter_map(|(anchor, spill)| {
                let id = self.cells[*anchor].id;
                (cell != id
                    && cell.sheet == id.sheet
                    && cell.row >= id.row
                    && (cell.row as usize) < id.row as usize + spill.rows
                    && cell.column >= id.column
                    && (cell.column as usize) < id.column as usize + spill.columns)
                    .then_some(*anchor)
            })
            .collect()
    }

    fn spill_member_value(&self, member: usize, anchor: usize) -> Value {
        let Some(spill) = self.spills.get(&anchor) else {
            return Value::Blank;
        };
        let id = self.cells[member].id;
        let anchor_id = self.cells[anchor].id;
        if id.sheet != anchor_id.sheet || id.row < anchor_id.row || id.column < anchor_id.column {
            return Value::Blank;
        }
        let row = (id.row - anchor_id.row) as usize;
        let column = (id.column - anchor_id.column) as usize;
        spill
            .values
            .get(row * spill.columns + column)
            .cloned()
            .unwrap_or(Value::Blank)
    }
}

fn push_resolved(expression: Expr<CellId>, binding: &mut Binding) {
    match expression {
        Expr::Reference(cell) => binding.cells.push(cell),
        Expr::Range {
            anchor,
            members,
            rows,
            columns,
        } => binding
            .ranges
            .push(range_key(anchor, members, rows, columns)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(row: u32, column: u32) -> CellId {
        CellId::new(0, row, column)
    }

    #[test]
    fn cached_rand_draws_replay_until_the_next_tick() {
        let mut workbook = Workbook::default();
        let draw = cell(0, 0);
        workbook.replay_cached_random(draw, 0.25);
        workbook.replay_cached_random(cell(1, 0), 4.0);
        workbook.set_formula(draw, "=RAND()").unwrap();
        workbook.set_formula(cell(0, 1), "=A1*4").unwrap();
        workbook
            .set_formula(cell(1, 0), "=RANDBETWEEN(1,6)")
            .unwrap();
        workbook.set_formula(cell(1, 1), "=A2*2").unwrap();
        assert_eq!(workbook.value(draw), Value::Number(0.25));
        assert_eq!(workbook.value(cell(0, 1)), Value::Number(1.0));
        assert_eq!(workbook.value(cell(1, 0)), Value::Number(4.0));
        assert_eq!(workbook.value(cell(1, 1)), Value::Number(8.0));
        workbook.replay_cached_random(cell(2, 0), 1.0);
        workbook.set_formula(cell(2, 0), "=RAND()").unwrap();
        assert_eq!(
            workbook.value(cell(2, 0)),
            Value::Error(CalcError::NotAvailable)
        );
        workbook.replay_cached_random(cell(2, 1), 9.0);
        workbook
            .set_formula(cell(2, 1), "=RANDBETWEEN(1,6)")
            .unwrap();
        assert_eq!(
            workbook.value(cell(2, 1)),
            Value::Error(CalcError::NotAvailable)
        );
        workbook.set_tick(0);
        assert_ne!(workbook.value(draw), Value::Number(0.25));
        match workbook.value(draw) {
            Value::Number(value) => assert!((0.0..1.0).contains(&value)),
            other => panic!("fresh rand {other:?}"),
        }
    }

    #[test]
    fn hard_formula_behaviors() {
        let mut workbook = Workbook::default();
        workbook.set_formula(cell(0, 0), "=TODAY()").unwrap();
        workbook.set_formula(cell(0, 1), "=NOW()").unwrap();
        workbook.set_formula(cell(0, 2), "=RAND()").unwrap();
        workbook
            .set_formula(cell(0, 3), "=RANDBETWEEN(10,12)")
            .unwrap();
        assert_eq!(
            workbook.value(cell(0, 0)),
            Value::Error(CalcError::NotAvailable)
        );
        assert_eq!(
            workbook.value(cell(0, 1)),
            Value::Error(CalcError::NotAvailable)
        );
        assert_eq!(
            workbook.value(cell(0, 2)),
            Value::Error(CalcError::NotAvailable)
        );
        workbook.set_tick(0);
        assert_eq!(workbook.value(cell(0, 0)), Value::Number(25_569.0));
        assert_eq!(workbook.value(cell(0, 1)), Value::Number(25_569.0));
        let first_rand = workbook.value(cell(0, 2));
        let first_between = workbook.value(cell(0, 3));
        workbook.install_tick(1, 0);
        assert_eq!(workbook.value(cell(0, 2)), first_rand);
        assert_eq!(workbook.value(cell(0, 3)), first_between);
        workbook.set_formula(cell(1, 2), "=RAND()").unwrap();
        assert_ne!(workbook.value(cell(1, 2)), first_rand);
        workbook.set_tick(43_200_000);
        assert_eq!(workbook.value(cell(0, 0)), Value::Number(25_569.0));
        assert_eq!(workbook.value(cell(0, 1)), Value::Number(25_569.5));
        assert_ne!(workbook.value(cell(0, 2)), first_rand);
        match first_between {
            Value::Number(value) => assert!((10.0..=12.0).contains(&value)),
            other => panic!("randbetween {other:?}"),
        }
        assert_eq!(
            workbook
                .set_formula(cell(2, 3), "=RANDBETWEEN(3,1)")
                .map(|_| workbook.value(cell(2, 3))),
            Ok(Value::Error(CalcError::InvalidNumber))
        );
        workbook
            .set_formula(cell(3, 3), "=RANDBETWEEN(1.9,3.2)")
            .unwrap();
        match workbook.value(cell(3, 3)) {
            Value::Number(value) => assert!(
                value.fract() == 0.0 && (1.0..=3.0).contains(&value),
                "{value}"
            ),
            other => panic!("truncated randbetween {other:?}"),
        }
        workbook
            .set_formula(cell(3, 4), "=RANDBETWEEN(\"1\",\"3\")")
            .unwrap();
        match workbook.value(cell(3, 4)) {
            Value::Number(value) => assert!((1.0..=3.0).contains(&value)),
            other => panic!("numeric text randbetween {other:?}"),
        }
        assert_eq!(
            workbook
                .set_formula(cell(3, 5), "=RANDBETWEEN(\"x\",1)")
                .map(|_| workbook.value(cell(3, 5))),
            Ok(Value::Error(CalcError::InvalidValue))
        );
        workbook
            .set_formula(cell(4, 0), "=RANDBETWEEN(-1.9,0.2)")
            .unwrap();
        for step in 0..40 {
            workbook.set_tick(step * 86_400_000);
            match workbook.value(cell(4, 0)) {
                Value::Number(value) => assert!(
                    value.fract() == 0.0 && (-1.0..=0.0).contains(&value),
                    "{value}"
                ),
                other => panic!("negative truncation {other:?}"),
            }
        }

        workbook.set_number(cell(4, 1), 9.0);
        workbook.set_formula(cell(4, 0), "={1,2;3,4}").unwrap();
        assert_eq!(workbook.value(cell(4, 0)), Value::Error(CalcError::Spill));
        assert_eq!(workbook.value(cell(4, 1)), Value::Number(9.0));
        workbook.clear(cell(4, 1));
        assert_eq!(workbook.value(cell(4, 0)), Value::Number(1.0));
        assert_eq!(workbook.value(cell(4, 1)), Value::Number(2.0));
        assert_eq!(workbook.value(cell(5, 0)), Value::Number(3.0));
        assert_eq!(workbook.value(cell(5, 1)), Value::Number(4.0));
        workbook.set_formula(cell(4, 2), "=B5").unwrap();
        assert_eq!(workbook.value(cell(4, 2)), Value::Number(2.0));
        workbook.set_formula(cell(4, 0), "={5,6;7,8}").unwrap();
        assert_eq!(workbook.value(cell(4, 2)), Value::Number(6.0));
        workbook.set_number(cell(0, 4), 1.0);
        workbook.set_number(cell(0, 5), 2.0);
        workbook
            .set_formula(cell(1, 4), "=TRANSPOSE(E1:F1)")
            .unwrap();
        assert_eq!(workbook.value(cell(1, 4)), Value::Number(1.0));
        assert_eq!(workbook.value(cell(2, 4)), Value::Number(2.0));
        workbook.set_number(cell(0, 4), 5.0);
        assert_eq!(workbook.value(cell(1, 4)), Value::Number(5.0));
        workbook
            .set_formula(cell(0, MAX_COLUMNS - 1), "={1,2}")
            .unwrap();
        assert_eq!(
            workbook.value(cell(0, MAX_COLUMNS - 1)),
            Value::Error(CalcError::Spill)
        );

        workbook.set_number(cell(8, 0), 3.0);
        workbook.set_number(cell(9, 0), 4.0);
        workbook.set_number(cell(9, 1), 8.0);
        workbook
            .set_formula(cell(10, 0), "=OFFSET(A9,1,1)")
            .unwrap();
        assert_eq!(workbook.value(cell(10, 0)), Value::Number(8.0));
        workbook
            .set_formula(cell(10, 1), "=SUM(OFFSET(A9,0,0,2,1))")
            .unwrap();
        assert_eq!(workbook.value(cell(10, 1)), Value::Number(7.0));
        assert_eq!(workbook.value(cell(10, 2)), Value::Blank);
        workbook
            .set_formula(cell(10, 2), "=OFFSET(A9,-20,0)")
            .unwrap();
        assert_eq!(
            workbook.value(cell(10, 2)),
            Value::Error(CalcError::InvalidReference)
        );
        workbook
            .set_formula(cell(10, 3), "=OFFSET(A9,0,0,0,1)")
            .unwrap();
        assert_eq!(
            workbook.value(cell(10, 3)),
            Value::Error(CalcError::InvalidReference)
        );
        workbook.set_number(cell(11, 0), 1.0);
        workbook
            .set_formula(cell(11, 1), "=OFFSET(A9,A12,0)")
            .unwrap();
        assert_eq!(workbook.value(cell(11, 1)), Value::Number(4.0));
        workbook.set_number(cell(9, 0), 6.0);
        assert_eq!(workbook.value(cell(11, 1)), Value::Number(6.0));
        workbook.set_number(cell(11, 0), 2.0);
        workbook.set_number(cell(10, 0), 11.0);
        assert_eq!(workbook.value(cell(11, 1)), Value::Number(11.0));

        workbook.set_number(cell(14, 0), 6.0);
        workbook
            .set_formula(cell(14, 1), "=INDIRECT(\"A15\")")
            .unwrap();
        assert_eq!(workbook.value(cell(14, 1)), Value::Number(6.0));
        workbook.set_number(cell(14, 0), 7.0);
        assert_eq!(workbook.value(cell(14, 1)), Value::Number(7.0));
        workbook.set_text(cell(15, 2), "A15");
        workbook.set_formula(cell(15, 1), "=INDIRECT(C16)").unwrap();
        assert_eq!(workbook.value(cell(15, 1)), Value::Number(7.0));
        workbook.set_number(cell(15, 0), 4.0);
        workbook.set_text(cell(15, 2), "A16");
        assert_eq!(workbook.value(cell(15, 1)), Value::Number(4.0));
        workbook
            .set_formula(cell(16, 0), "=INDIRECT(\"R1C1\")")
            .unwrap();
        assert_eq!(
            workbook.value(cell(16, 0)),
            Value::Error(CalcError::InvalidReference)
        );
        workbook
            .set_formula(cell(16, 1), "=INDIRECT(\"Sheet1:Sheet2!A1\")")
            .unwrap();
        assert_eq!(
            workbook.value(cell(16, 1)),
            Value::Error(CalcError::InvalidReference)
        );
        workbook.set_text(cell(16, 2), "A17");
        assert!(
            workbook
                .set_formula(cell(16, 0), "=INDIRECT(C17)")
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
        workbook.set_number(cell(17, 0), 3.0);
        workbook.set_number(cell(18, 0), 4.0);
        workbook
            .set_formula(cell(17, 1), "=SUM(INDIRECT(\"A18:A19\"))")
            .unwrap();
        assert_eq!(workbook.value(cell(17, 1)), Value::Number(7.0));
        workbook.set_text(cell(20, 0), "B21");
        workbook.set_number(cell(20, 1), 9.0);
        workbook
            .set_formula(cell(21, 0), "=INDIRECT(A21&\"\")")
            .unwrap();
        assert_eq!(workbook.value(cell(21, 0)), Value::Number(9.0));
        workbook.set_number(cell(20, 1), 12.0);
        assert_eq!(workbook.value(cell(21, 0)), Value::Number(12.0));
    }
}
