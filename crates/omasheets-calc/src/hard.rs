//! Tick-backed volatiles, bounded spill, and dynamic OFFSET/INDIRECT.
use super::*;

#[derive(Clone, Debug)]
pub(super) struct SpillRecord {
    rows: usize,
    columns: usize,
    values: Vec<Value>,
    members: Vec<usize>,
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
        let first = parse_a1(left.trim(), sheet).ok()?;
        let second = parse_a1(right.trim(), sheet).ok()?;
        return expand_range(first, second).ok();
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
            let bottom = match number(self.evaluate(&arguments[0])) {
                Ok(value) => value,
                Err(error) => return Value::Error(error),
            };
            let top = match number(self.evaluate(&arguments[1])) {
                Ok(value) => value,
                Err(error) => return Value::Error(error),
            };
            let Some((tick, _)) = self.tick else {
                return Value::Error(CalcError::NotAvailable);
            };
            return match rand_between(volatile_unit(tick, self.evaluating.get()), bottom, top) {
                Ok(value) => number_value(value),
                Err(error) => Value::Error(error),
            };
        }
        let Some((tick, at)) = self.tick else {
            return Value::Error(CalcError::NotAvailable);
        };
        match function {
            Function::Today => match serial_date::serial_from_unix_millis(at) {
                Ok(serial) => Value::Number(serial.trunc()),
                Err(error) => Value::Error(error),
            },
            Function::Now => match serial_date::serial_from_unix_millis(at) {
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
            Expr::Reference(cell) => match self.value(*cell) {
                Value::Number(value) if value.is_finite() => Some(value),
                Value::Blank => Some(0.0),
                Value::Boolean(value) => Some(if value { 1.0 } else { 0.0 }),
                _ => None,
            },
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
        let waiting = indices
            .iter()
            .copied()
            .filter(|dependency| {
                self.dirty_marks.get(*dependency).copied() == Some(generation)
                    && self.eval_marks.get(*dependency).copied() != Some(generation)
                    && !matches!(
                        self.cells[*dependency].input,
                        Input::Range { .. } | Input::Tick
                    )
            })
            .count();
        if waiting == 0 {
            return false;
        }
        self.pending[formula] = waiting;
        true
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
        let preview = match &expression {
            Expr::Array(array) if array.rows.saturating_mul(array.columns) > 1 => {
                SpillPreview::Array(array.clone())
            }
            Expr::Function(function @ (Function::Transpose | Function::MMult), arguments) => {
                match self.matrix_array(*function, arguments) {
                    Ok(array) if array.rows.saturating_mul(array.columns) > 1 => {
                        SpillPreview::Array(array)
                    }
                    Ok(array) => SpillPreview::Scalar(
                        array.values.into_iter().next().unwrap_or(Value::Blank),
                    ),
                    Err(error) => SpillPreview::Scalar(Value::Error(error)),
                }
            }
            other => SpillPreview::Scalar(self.evaluate(other)),
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
    }
}
