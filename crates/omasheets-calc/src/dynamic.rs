//! `ROWS`, `CELL`, and the dynamic-array functions `FILTER`, `UNIQUE`, `SORT`,
//! and `LINEST`. Results wider than one cell spill from the formula cell.
use super::*;

impl Workbook {
    pub(super) fn array_result(
        &self,
        function: Function,
        arguments: &[Expr<usize>],
    ) -> Result<ArrayValue, CalcError> {
        match function {
            Function::Transpose | Function::MMult => self.matrix_array(function, arguments),
            Function::Filter => self.filter_array(arguments),
            Function::Unique => self.unique_array(arguments),
            Function::Sort => self.sort_array(arguments),
            Function::Linest => self.linest_array(arguments),
            _ => Err(CalcError::InvalidArguments),
        }
    }

    pub(super) fn rows_value(&self, arguments: &[Expr<usize>]) -> Value {
        let [argument] = arguments else {
            return Value::Error(CalcError::InvalidArguments);
        };
        let rows = if let Ok(view) = self.reference_view(argument) {
            view.rows
        } else if argument_can_be_array(argument) {
            match self.evaluate_array(argument) {
                Ok(array) => array.rows,
                Err(error) => return Value::Error(error),
            }
        } else {
            return Value::Error(CalcError::InvalidValue);
        };
        Value::Number(rows as f64)
    }

    pub(super) fn cell_info(&self, arguments: &[Expr<usize>]) -> Value {
        if !matches!(arguments.len(), 1 | 2) {
            return Value::Error(CalcError::InvalidArguments);
        }
        let kind = match text_value(&self.evaluate(&arguments[0])) {
            Ok(text) => text.trim().to_ascii_lowercase(),
            Err(error) => return Value::Error(error),
        };
        let Some(info) = cell_info_kind(&kind) else {
            return Value::Error(CalcError::InvalidValue);
        };
        let id = match arguments.get(1) {
            None => self.evaluating.get(),
            Some(argument) => match self.reference_view(argument) {
                Ok(view) => view.origin(self).0,
                Err(error) => return Value::Error(error),
            },
        };
        let value = self.value(id);
        match info {
            "address" => Value::Text(format!("${}${}", column_letters(id.column), id.row + 1)),
            "col" => Value::Number(f64::from(id.column) + 1.0),
            "row" => Value::Number(f64::from(id.row) + 1.0),
            "contents" => value,
            "type" => Value::Text(match value {
                Value::Blank => "b".into(),
                Value::Text(_) => "l".into(),
                _ => "v".into(),
            }),
            "filename" => Value::Text(String::new()),
            "format" => Value::Text("G".into()),
            "color" | "parentheses" => Value::Number(0.0),
            "prefix" => Value::Text(String::new()),
            "protect" => Value::Number(1.0),
            "width" => Value::Number(8.0),
            _ => Value::Error(CalcError::InvalidValue),
        }
    }

    fn filter_array(&self, arguments: &[Expr<usize>]) -> Result<ArrayValue, CalcError> {
        if !matches!(arguments.len(), 2 | 3) {
            return Err(CalcError::InvalidArguments);
        }
        let array = self.evaluate_array(&arguments[0])?;
        let include = self.evaluate_array(&arguments[1])?;
        let by_row = include.rows == array.rows && include.columns == 1;
        let by_column = include.columns == array.columns && include.rows == 1;
        if !by_row && !by_column {
            return Err(CalcError::InvalidValue);
        }
        let mut values = Vec::new();
        if by_row {
            for row in 0..array.rows {
                if !include_flag(include.at(row, 0))? {
                    continue;
                }
                for column in 0..array.columns {
                    values.push(array.at(row, column));
                }
            }
            if values.is_empty() {
                return self.empty_filter(arguments.get(2));
            }
            let rows = values.len() / array.columns;
            return Ok(ArrayValue {
                rows,
                columns: array.columns,
                values,
            });
        }
        for column in 0..array.columns {
            if !include_flag(include.at(0, column))? {
                continue;
            }
            for row in 0..array.rows {
                values.push(array.at(row, column));
            }
        }
        if values.is_empty() {
            return self.empty_filter(arguments.get(2));
        }
        let columns = values.len() / array.rows;
        let mut ordered = Vec::with_capacity(values.len());
        for row in 0..array.rows {
            for column in 0..columns {
                ordered.push(values[column * array.rows + row].clone());
            }
        }
        Ok(ArrayValue {
            rows: array.rows,
            columns,
            values: ordered,
        })
    }

    fn empty_filter(&self, if_empty: Option<&Expr<usize>>) -> Result<ArrayValue, CalcError> {
        match if_empty {
            Some(expression) => self.evaluate_array(expression),
            None => Err(CalcError::Calculation),
        }
    }

    fn unique_array(&self, arguments: &[Expr<usize>]) -> Result<ArrayValue, CalcError> {
        if arguments.is_empty() || arguments.len() > 3 {
            return Err(CalcError::InvalidArguments);
        }
        let array = self.evaluate_array(&arguments[0])?;
        let by_column = match arguments.get(1) {
            None | Some(Expr::Empty) => false,
            Some(expression) => truthy(self.evaluate(expression))?,
        };
        let exactly_once = match arguments.get(2) {
            None | Some(Expr::Empty) => false,
            Some(expression) => truthy(self.evaluate(expression))?,
        };
        if by_column {
            let transposed = transpose_array(&array);
            let unique = unique_rows(&transposed, exactly_once)?;
            return Ok(transpose_array(&unique));
        }
        unique_rows(&array, exactly_once)
    }

    fn sort_array(&self, arguments: &[Expr<usize>]) -> Result<ArrayValue, CalcError> {
        if arguments.is_empty() || arguments.len() > 4 {
            return Err(CalcError::InvalidArguments);
        }
        let array = self.evaluate_array(&arguments[0])?;
        let index = match arguments.get(1) {
            None | Some(Expr::Empty) => 1.0,
            Some(expression) => number(self.evaluate(expression))?,
        };
        let order = match arguments.get(2) {
            None | Some(Expr::Empty) => 1.0,
            Some(expression) => number(self.evaluate(expression))?,
        };
        let by_column = match arguments.get(3) {
            None | Some(Expr::Empty) => false,
            Some(expression) => truthy(self.evaluate(expression))?,
        };
        if order != 1.0 && order != -1.0 {
            return Err(CalcError::InvalidValue);
        }
        let index = index.trunc() as isize;
        if by_column {
            if index < 1 || index as usize > array.rows {
                return Err(CalcError::InvalidValue);
            }
            let transposed = transpose_array(&array);
            let sorted = sort_rows(&transposed, index as usize - 1, order < 0.0)?;
            return Ok(transpose_array(&sorted));
        }
        if index < 1 || index as usize > array.columns {
            return Err(CalcError::InvalidValue);
        }
        sort_rows(&array, index as usize - 1, order < 0.0)
    }

    fn linest_array(&self, arguments: &[Expr<usize>]) -> Result<ArrayValue, CalcError> {
        if arguments.is_empty() || arguments.len() > 4 {
            return Err(CalcError::InvalidArguments);
        }
        let y_array = self.evaluate_array(&arguments[0])?;
        let y = observations(&y_array)?;
        let x = match arguments.get(1) {
            None | Some(Expr::Empty) => {
                (0..y.len()).map(|index| vec![(index + 1) as f64]).collect()
            }
            Some(expression) => predictors(&self.evaluate_array(expression)?, y.len())?,
        };
        let with_intercept = match arguments.get(2) {
            None | Some(Expr::Empty) => true,
            Some(expression) => truthy(self.evaluate(expression))?,
        };
        let stats = match arguments.get(3) {
            None | Some(Expr::Empty) => false,
            Some(expression) => truthy(self.evaluate(expression))?,
        };
        linear_fit(&y, &x, with_intercept, stats)
    }
}

fn argument_can_be_array(expression: &Expr<usize>) -> bool {
    matches!(
        expression,
        Expr::Array(_)
            | Expr::Local(_)
            | Expr::RangeNode { .. }
            | Expr::Function(
                Function::Transpose
                    | Function::MMult
                    | Function::Filter
                    | Function::Unique
                    | Function::Sort
                    | Function::Linest
                    | Function::Offset
                    | Function::Indirect
                    | Function::Index
                    | Function::ReferenceSpan,
                _
            )
    )
}

fn cell_info_kind(text: &str) -> Option<&'static str> {
    const KINDS: [&str; 12] = [
        "address",
        "col",
        "color",
        "contents",
        "filename",
        "format",
        "parentheses",
        "prefix",
        "protect",
        "row",
        "type",
        "width",
    ];
    if text.is_empty() {
        return None;
    }
    if let Some(exact) = KINDS.into_iter().find(|kind| *kind == text) {
        return Some(exact);
    }
    let matches: Vec<_> = KINDS
        .into_iter()
        .filter(|kind| kind.starts_with(text))
        .collect();
    (matches.len() == 1).then_some(matches[0])
}

fn column_letters(mut column: u32) -> String {
    let mut letters = Vec::new();
    column += 1;
    while column > 0 {
        column -= 1;
        letters.push(b'A' + (column % 26) as u8);
        column /= 26;
    }
    letters.reverse();
    String::from_utf8(letters).unwrap_or_default()
}

fn include_flag(value: Value) -> Result<bool, CalcError> {
    match value {
        Value::Boolean(flag) => Ok(flag),
        Value::Blank => Ok(false),
        Value::Error(error) => Err(error),
        _ => Err(CalcError::InvalidValue),
    }
}

fn transpose_array(array: &ArrayValue) -> ArrayValue {
    let mut values = Vec::with_capacity(array.values.len());
    for column in 0..array.columns {
        for row in 0..array.rows {
            values.push(array.at(row, column));
        }
    }
    ArrayValue {
        rows: array.columns,
        columns: array.rows,
        values,
    }
}

fn row_slice(array: &ArrayValue, row: usize) -> Vec<Value> {
    (0..array.columns)
        .map(|column| array.at(row, column))
        .collect()
}

fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left == right,
        (Value::Text(left), Value::Text(right)) => left == right,
        (Value::Boolean(left), Value::Boolean(right)) => left == right,
        (Value::Blank, Value::Blank) => true,
        (Value::Error(left), Value::Error(right)) => left == right,
        _ => false,
    }
}

fn rows_equal(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| values_equal(left, right))
}

fn unique_rows(array: &ArrayValue, exactly_once: bool) -> Result<ArrayValue, CalcError> {
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut counts = Vec::new();
    for row in 0..array.rows {
        let values = row_slice(array, row);
        if let Some(error) = values.iter().find_map(|value| match value {
            Value::Error(error) => Some(error.clone()),
            _ => None,
        }) {
            return Err(error);
        }
        if let Some(index) = rows.iter().position(|kept| rows_equal(kept, &values)) {
            counts[index] += 1;
        } else {
            rows.push(values);
            counts.push(1);
        }
    }
    let kept: Vec<_> = rows
        .into_iter()
        .zip(counts)
        .filter(|(_, count)| !exactly_once || *count == 1)
        .map(|(row, _)| row)
        .collect();
    if kept.is_empty() {
        return Err(CalcError::Calculation);
    }
    let mut values = Vec::new();
    for row in &kept {
        values.extend(row.iter().cloned());
    }
    Ok(ArrayValue {
        rows: kept.len(),
        columns: array.columns,
        values,
    })
}

fn sort_rank(value: &Value) -> Result<u8, CalcError> {
    match value {
        Value::Number(_) => Ok(0),
        Value::Text(_) => Ok(1),
        Value::Boolean(_) => Ok(2),
        Value::Blank => Ok(3),
        Value::Error(error) => Err(error.clone()),
    }
}

fn sort_compare(
    left: &Value,
    right: &Value,
    descending: bool,
) -> Result<std::cmp::Ordering, CalcError> {
    if matches!(left, Value::Blank) || matches!(right, Value::Blank) {
        return Ok(match (left, right) {
            (Value::Blank, Value::Blank) => std::cmp::Ordering::Equal,
            (Value::Blank, _) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Less,
        });
    }
    let order = sort_rank(left)?.cmp(&sort_rank(right)?);
    let order = if order != std::cmp::Ordering::Equal {
        order
    } else {
        match (left, right) {
            (Value::Number(left), Value::Number(right)) => {
                left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
            }
            (Value::Text(left), Value::Text(right)) => {
                left.to_lowercase().cmp(&right.to_lowercase())
            }
            (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
            _ => std::cmp::Ordering::Equal,
        }
    };
    Ok(if descending { order.reverse() } else { order })
}

fn sort_rows(array: &ArrayValue, key: usize, descending: bool) -> Result<ArrayValue, CalcError> {
    let mut order: Vec<usize> = (0..array.rows).collect();
    let mut failure = None;
    order.sort_by(|left, right| {
        if failure.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match sort_compare(&array.at(*left, key), &array.at(*right, key), descending) {
            Ok(order) => order,
            Err(error) => {
                failure = Some(error);
                std::cmp::Ordering::Equal
            }
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    let mut values = Vec::with_capacity(array.values.len());
    for row in order {
        values.extend(row_slice(array, row));
    }
    Ok(ArrayValue {
        rows: array.rows,
        columns: array.columns,
        values,
    })
}

fn observations(array: &ArrayValue) -> Result<Vec<f64>, CalcError> {
    let along_row = array.rows == 1 && array.columns > 1;
    let count = if along_row { array.columns } else { array.rows };
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        let value = if along_row {
            array.at(0, index)
        } else if array.columns == 1 {
            array.at(index, 0)
        } else {
            return Err(CalcError::InvalidReference);
        };
        values.push(number(value)?);
    }
    if values.is_empty() {
        return Err(CalcError::InvalidReference);
    }
    Ok(values)
}

fn predictors(array: &ArrayValue, observations: usize) -> Result<Vec<Vec<f64>>, CalcError> {
    if array.rows == 1 && array.columns == observations {
        return (0..observations)
            .map(|index| Ok(vec![number(array.at(0, index))?]))
            .collect();
    }
    if array.rows != observations {
        return Err(CalcError::InvalidReference);
    }
    let mut rows = Vec::with_capacity(observations);
    for row in 0..array.rows {
        let mut predictors = Vec::with_capacity(array.columns);
        for column in 0..array.columns {
            predictors.push(number(array.at(row, column))?);
        }
        rows.push(predictors);
    }
    Ok(rows)
}

fn linear_fit(
    y: &[f64],
    x: &[Vec<f64>],
    with_intercept: bool,
    stats: bool,
) -> Result<ArrayValue, CalcError> {
    if x.len() != y.len() || x.is_empty() || x[0].is_empty() {
        return Err(CalcError::InvalidReference);
    }
    let variables = x[0].len();
    let coefficients = variables + usize::from(with_intercept);
    if y.len() < coefficients {
        return Err(CalcError::InvalidNumber);
    }
    let width = coefficients;
    let mut normal = vec![vec![0.0; width]; width];
    let mut target = vec![0.0; width];
    for (row_y, predictors) in y.iter().zip(x) {
        let mut design = predictors.clone();
        if with_intercept {
            design.push(1.0);
        }
        // Excel lists slopes from the last x column back to the first.
        if with_intercept {
            design[..variables].reverse();
        } else {
            design.reverse();
        }
        for row in 0..width {
            target[row] += design[row] * row_y;
            for column in 0..width {
                normal[row][column] += design[row] * design[column];
            }
        }
    }
    let beta = solve(&mut normal, &target).ok_or(CalcError::InvalidNumber)?;
    if !stats {
        return Ok(ArrayValue {
            rows: 1,
            columns: beta.len(),
            values: beta.into_iter().map(Value::Number).collect(),
        });
    }
    let inverse = invert(&normal).ok_or(CalcError::InvalidNumber)?;
    let mut residual = 0.0;
    let mut total = 0.0;
    let mean = y.iter().sum::<f64>() / y.len() as f64;
    for (row_y, predictors) in y.iter().zip(x) {
        let mut design = predictors.clone();
        if with_intercept {
            design.push(1.0);
            design[..variables].reverse();
        } else {
            design.reverse();
        }
        let estimate: f64 = design.iter().zip(&beta).map(|(x, b)| x * b).sum();
        let delta = row_y - estimate;
        residual += delta * delta;
        let centered = if with_intercept { row_y - mean } else { *row_y };
        total += centered * centered;
    }
    let df = y.len() as f64 - coefficients as f64;
    let model_df = if with_intercept {
        variables as f64
    } else {
        coefficients as f64
    };
    let sigma = if df > 0.0 { residual / df } else { f64::NAN };
    let r2 = if total == 0.0 {
        if residual == 0.0 { 1.0 } else { f64::NAN }
    } else {
        1.0 - residual / total
    };
    let f_stat = if df > 0.0 && model_df > 0.0 && residual != 0.0 {
        (total - residual) / model_df / (residual / df)
    } else {
        f64::NAN
    };
    let sey = sigma.sqrt();
    let mut values = Vec::new();
    values.extend(beta.iter().copied().map(Value::Number));
    for (index, _) in beta.iter().enumerate() {
        let variance = inverse[index][index] * sigma;
        values.push(stat_value(variance.sqrt()));
    }
    values.push(stat_value(r2));
    values.push(stat_value(sey));
    for _ in 2..beta.len() {
        values.push(Value::Blank);
    }
    values.push(stat_value(f_stat));
    values.push(stat_value(df));
    for _ in 2..beta.len() {
        values.push(Value::Blank);
    }
    values.push(stat_value(total - residual));
    values.push(stat_value(residual));
    for _ in 2..beta.len() {
        values.push(Value::Blank);
    }
    Ok(ArrayValue {
        rows: 5,
        columns: beta.len(),
        values,
    })
}

fn stat_value(value: f64) -> Value {
    if value.is_finite() {
        Value::Number(value)
    } else {
        Value::Error(CalcError::InvalidNumber)
    }
}

fn solve(matrix: &mut [Vec<f64>], target: &[f64]) -> Option<Vec<f64>> {
    let size = matrix.len();
    let mut system = matrix.to_vec();
    let mut answer = target.to_vec();
    for column in 0..size {
        let mut pivot = column;
        for row in column + 1..size {
            if system[row][column].abs() > system[pivot][column].abs() {
                pivot = row;
            }
        }
        if system[pivot][column].abs() < 1e-12 {
            return None;
        }
        system.swap(column, pivot);
        answer.swap(column, pivot);
        let scale = system[column][column];
        for value in &mut system[column] {
            *value /= scale;
        }
        answer[column] /= scale;
        for row in 0..size {
            if row == column {
                continue;
            }
            let factor = system[row][column];
            for col in 0..size {
                system[row][col] -= factor * system[column][col];
            }
            answer[row] -= factor * answer[column];
        }
    }
    Some(answer)
}

fn invert(matrix: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let size = matrix.len();
    let mut inverse = vec![vec![0.0; size]; size];
    for column in 0..size {
        let mut identity = vec![0.0; size];
        identity[column] = 1.0;
        let solved = solve(&mut matrix.to_vec(), &identity)?;
        for row in 0..size {
            inverse[row][column] = solved[row];
        }
    }
    Some(inverse)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(row: u32, column: u32) -> CellId {
        CellId::new(0, row, column)
    }

    #[test]
    fn rows_cell_and_dynamic_arrays_follow_excel() {
        let mut workbook = Workbook::default();
        workbook.set_number(cell(0, 0), 3.0);
        workbook.set_number(cell(1, 0), 1.0);
        workbook.set_number(cell(2, 0), 2.0);
        workbook.set_boolean(cell(0, 1), true);
        workbook.set_boolean(cell(1, 1), false);
        workbook.set_boolean(cell(2, 1), true);
        workbook.set_text(cell(0, 2), "b");
        workbook.set_text(cell(1, 2), "a");
        workbook.set_text(cell(2, 2), "b");
        workbook.set_formula(cell(0, 4), "=ROWS(A1:C2)").unwrap();
        workbook
            .set_formula(cell(1, 4), "=CELL(\"row\",B2)")
            .unwrap();
        workbook
            .set_formula(cell(2, 4), "=CELL(\"col\",B2)")
            .unwrap();
        workbook
            .set_formula(cell(3, 4), "=CELL(\"address\",B2)")
            .unwrap();
        workbook
            .set_formula(cell(4, 4), "=CELL(\"type\",C1)")
            .unwrap();
        workbook
            .set_formula(cell(5, 4), "=SUM(FILTER(A1:A3,B1:B3))")
            .unwrap();
        workbook.set_formula(cell(6, 6), "=UNIQUE(C1:C3)").unwrap();
        workbook.set_formula(cell(6, 4), "=SORT(A1:A3)").unwrap();
        workbook.set_formula(cell(8, 6), "=LINEST(A1:A3)").unwrap();
        workbook
            .set_formula(cell(0, 5), "=FILTER(A1:A3,B1:B3)")
            .unwrap();
        assert_eq!(workbook.value(cell(0, 4)), Value::Number(2.0));
        assert_eq!(workbook.value(cell(1, 4)), Value::Number(2.0));
        assert_eq!(workbook.value(cell(2, 4)), Value::Number(2.0));
        assert_eq!(workbook.value(cell(3, 4)), Value::Text("$B$2".into()));
        assert_eq!(workbook.value(cell(4, 4)), Value::Text("l".into()));
        assert_eq!(workbook.value(cell(5, 4)), Value::Number(5.0));
        assert_eq!(workbook.value(cell(6, 6)), Value::Text("b".into()));
        assert_eq!(workbook.value(cell(7, 6)), Value::Text("a".into()));
        assert_eq!(workbook.value(cell(6, 4)), Value::Number(1.0));
        assert_eq!(workbook.value(cell(7, 4)), Value::Number(2.0));
        let Value::Number(slope) = workbook.value(cell(8, 6)) else {
            panic!("linest {:?}", workbook.value(cell(8, 6)));
        };
        assert!((slope + 0.5).abs() < 1e-9, "{slope}");
        assert_eq!(workbook.value(cell(0, 5)), Value::Number(3.0));
        assert_eq!(workbook.value(cell(1, 5)), Value::Number(2.0));
    }

    #[test]
    fn arithmetic_coerces_numeric_text() {
        let mut workbook = Workbook::default();
        workbook.set_text(cell(0, 0), "5");
        workbook.set_number(cell(0, 1), 16.0);
        workbook.set_formula(cell(0, 2), "=A1-B1").unwrap();
        workbook.set_formula(cell(0, 3), "=A1<10").unwrap();
        workbook.set_formula(cell(0, 4), "=\"PH\"+1").unwrap();
        workbook.set_formula(cell(0, 5), "=A1>10").unwrap();
        assert_eq!(workbook.value(cell(0, 2)), Value::Number(-11.0));
        // Text sorts after numbers, so the digits "5" are not less than 10.
        assert_eq!(workbook.value(cell(0, 3)), Value::Boolean(false));
        assert_eq!(workbook.value(cell(0, 5)), Value::Boolean(true));
        assert_eq!(
            workbook.value(cell(0, 4)),
            Value::Error(CalcError::InvalidValue)
        );
    }

    #[test]
    fn cancelled_difference_is_zero_at_excel_precision() {
        let mut workbook = Workbook::default();
        // These are the doubles Excel wrote for a gross-profit row whose
        // cached difference is 0. IEEE subtraction leaves -2^-42; Excel's
        // 15-digit values cancel, so IFERROR of a later quotient is "-".
        workbook.set_number(cell(0, 0), 1237.7868131999999);
        workbook.set_number(cell(0, 1), 501.00894820000008);
        workbook.set_number(cell(0, 2), 736.77786500000002);
        workbook.set_formula(cell(0, 3), "=A1-SUM(B1:C1)").unwrap();
        workbook
            .set_formula(cell(0, 4), "=IFERROR(75/D1,\"-\")")
            .unwrap();
        assert_eq!(workbook.value(cell(0, 3)), Value::Number(0.0));
        assert_eq!(workbook.value(cell(0, 4)), Value::Text("-".into()));
    }

    #[test]
    fn offset_from_its_own_cell_reads_the_shifted_cell() {
        let mut workbook = Workbook::default();
        // D1 is 2, so OFFSET(N1,$D$1,0) in N1 is N3, not a circular read of N1.
        workbook.set_number(cell(0, 3), 2.0);
        workbook.set_number(cell(2, 13), 0.84);
        workbook
            .set_formula(cell(0, 13), "=OFFSET(N1,$D$1,0)")
            .unwrap();
        assert_eq!(workbook.value(cell(0, 13)), Value::Number(0.84));
        // Import evaluates one pass at the end. The row shift is a literal
        // that is not visible through `value` until that pass.
        let mut bulk = Workbook::default();
        bulk.begin_bulk();
        bulk.set_number(cell(0, 3), 2.0);
        bulk.set_number(cell(2, 13), 0.84);
        bulk.set_formula(cell(0, 13), "=OFFSET(N1,$D$1,0)")
            .unwrap();
        bulk.end_bulk();
        assert_eq!(bulk.value(cell(0, 13)), Value::Number(0.84));
        assert!(matches!(
            workbook.set_formula(cell(4, 0), "=OFFSET(A5,0,0)"),
            Err(FormulaError::Cycle(_))
        ));
    }

    #[test]
    fn offset_waits_for_a_formula_in_the_shifted_cell() {
        let mut workbook = Workbook::default();
        workbook.define_sheet(0, "Line");
        workbook.define_sheet(1, "DCF");
        workbook.begin_bulk();
        // G1 is the case index. H191 shifts four rows to H195, and H195 is a
        // formula of the cell to its left, so it is not ready when H191's
        // only static input (G1) is.
        workbook.set_number(CellId::new(1, 0, 6), 4.0);
        workbook.set_number(cell(194, 5), 3.7);
        workbook.set_formula(cell(194, 6), "=F195").unwrap();
        workbook.set_formula(cell(194, 7), "=G195").unwrap();
        workbook
            .set_formula(cell(190, 7), "=OFFSET(H191,'DCF'!$G$1,,)")
            .unwrap();
        // The first three months are a formula chain. The sum's width is a
        // literal, so the sum becomes ready before the later months do.
        workbook.set_number(cell(3, 2), 3.0);
        workbook.set_number(cell(17, 8), 5.0);
        workbook.set_formula(cell(17, 9), "=I18+1").unwrap();
        workbook.set_formula(cell(17, 10), "=J18+1").unwrap();
        workbook
            .set_formula(cell(17, 2), "=SUM(OFFSET($I18,0,0,1,C4))")
            .unwrap();
        workbook.end_bulk();
        assert_eq!(workbook.value(cell(194, 7)), Value::Number(3.7));
        assert_eq!(workbook.value(cell(190, 7)), Value::Number(3.7));
        assert_eq!(workbook.value(cell(17, 2)), Value::Number(18.0));
    }

    #[test]
    fn linest_of_two_offsets_reads_the_rows_between_them() {
        let mut workbook = Workbook::default();
        // A1 = 4 and A2 = 2, so the window is rows 8 through 9 of the data
        // that starts on row 5. The formula sits in that same column.
        workbook.set_number(cell(0, 0), 4.0);
        workbook.set_number(cell(1, 0), 2.0);
        workbook.set_number(cell(7, 1), 1.0);
        workbook.set_number(cell(8, 1), 2.0);
        workbook.set_number(cell(7, 2), 3.0);
        workbook.set_number(cell(8, 2), 5.0);
        workbook
            .set_formula(
                cell(3, 2),
                "=LINEST(OFFSET(C$5,ABS($A$2-$A$1)+1,0):OFFSET(C$5,$A$1,0),OFFSET($B$5,ABS($A$2-$A$1)+1,0):OFFSET($B5,$A$1,0))",
            )
            .unwrap();
        let Value::Number(slope) = workbook.value(cell(3, 2)) else {
            panic!("{:?}", workbook.value(cell(3, 2)));
        };
        assert!((slope - 2.0).abs() < 1e-9, "{slope}");
        let mut bulk = Workbook::default();
        bulk.begin_bulk();
        bulk.set_number(cell(0, 0), 4.0);
        bulk.set_number(cell(1, 0), 2.0);
        bulk.set_number(cell(7, 1), 1.0);
        bulk.set_number(cell(8, 1), 2.0);
        bulk.set_number(cell(7, 2), 3.0);
        bulk.set_number(cell(8, 2), 5.0);
        bulk.set_formula(
            cell(3, 2),
            "=LINEST(OFFSET(C$5,ABS($A$2-$A$1)+1,0):OFFSET(C$5,$A$1,0),OFFSET($B$5,ABS($A$2-$A$1)+1,0):OFFSET($B5,$A$1,0))",
        )
        .unwrap();
        bulk.end_bulk();
        let Value::Number(bulk_slope) = bulk.value(cell(3, 2)) else {
            panic!("{:?}", bulk.value(cell(3, 2)));
        };
        assert!((bulk_slope - 2.0).abs() < 1e-9, "{bulk_slope}");
    }

    #[test]
    fn a_name_can_span_from_a_cell_to_a_shifted_cell() {
        let mut workbook = Workbook::default();
        workbook.define_name("Top", "A1");
        workbook.define_name("Shift", "C1");
        workbook.define_name("Bot", "OFFSET(Top,Shift-1,0)");
        workbook.define_name("Span", "Top:Bot");
        workbook.set_number(cell(0, 0), 1.0);
        workbook.set_number(cell(1, 0), 2.0);
        workbook.set_number(cell(2, 0), 4.0);
        workbook.set_number(cell(0, 2), 3.0);
        workbook.set_formula(cell(0, 1), "=SUM(Span)").unwrap();
        workbook.set_formula(cell(1, 1), "=Bot").unwrap();
        workbook.set_formula(cell(2, 1), "=Top").unwrap();
        workbook.set_formula(cell(3, 1), "=ROWS(Span)").unwrap();
        workbook
            .set_formula(cell(4, 1), "=CELL(\"address\",Span)")
            .unwrap();
        assert_eq!(
            (
                workbook.value(cell(0, 1)),
                workbook.value(cell(1, 1)),
                workbook.value(cell(2, 1)),
                workbook.value(cell(3, 1)),
                workbook.value(cell(4, 1)),
            ),
            (
                Value::Number(7.0),
                Value::Number(4.0),
                Value::Number(1.0),
                Value::Number(3.0),
                Value::Text("$A$1".into()),
            )
        );
    }
}
