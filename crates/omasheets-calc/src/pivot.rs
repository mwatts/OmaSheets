//! `GETPIVOTDATA` over a pivot cache loaded with the workbook.
//!
//! Dates in the cache are Excel 1900 serials. This module never reads the
//! system clock, so relative filters (`today`, `thisMonth`) are not applied.
//! Count, average, min, max, product, and the statistical subtotals are not
//! implemented: those data fields return `#VALUE!`.

use super::*;
use crate::serial_date::{civil_from_serial, serial_from_civil};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// One cached field. `shared` is the shared-item list, or the group labels
/// when [`PivotCacheField::group`] is set. Record values are already resolved.
#[derive(Clone, Debug)]
pub struct PivotCacheField {
    pub name: String,
    pub shared: Vec<PivotScalar>,
    pub group: Option<PivotDateGroup>,
}

/// A cache field grouped from a base date field. Only year, quarter, and
/// month groupings are recognised; anything else is left unset.
#[derive(Clone, Debug)]
pub struct PivotDateGroup {
    pub base: usize,
    pub by: PivotGroupBy,
    pub start: f64,
    pub end: f64,
    pub items: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PivotGroupBy {
    Years,
    Quarters,
    Months,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PivotScalar {
    Blank,
    Number(f64),
    Text(String),
}

#[derive(Clone, Debug)]
pub struct PivotCache {
    pub fields: Vec<PivotCacheField>,
    pub records: Vec<Vec<PivotScalar>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PivotAggregate {
    Sum,
    /// Not calculated. `GETPIVOTDATA` returns `#VALUE!`.
    Unsupported,
}

#[derive(Clone, Debug)]
pub struct PivotDataField {
    pub name: String,
    pub source: usize,
    pub aggregate: PivotAggregate,
}

/// Inclusive or exclusive numeric bounds from a `dateBetween` filter.
/// Compared as Excel serials. `negated` is `dateNotBetween`.
#[derive(Clone, Debug)]
pub struct PivotDateFilter {
    pub field: usize,
    pub low: f64,
    pub high: f64,
    pub low_inclusive: bool,
    pub high_inclusive: bool,
    pub negated: bool,
}

#[derive(Clone, Debug)]
pub struct PivotTable {
    pub sheet: u32,
    pub first_row: u32,
    pub last_row: u32,
    pub first_column: u32,
    pub last_column: u32,
    pub cache: usize,
    pub data_fields: Vec<PivotDataField>,
    /// Row, column, and page fields. A date filter applies only to these.
    pub axis_fields: Vec<usize>,
    pub filters: Vec<PivotDateFilter>,
    /// Shared-item indexes that remain visible. One entry per restricted field.
    pub visible_items: Vec<(usize, Vec<usize>)>,
}

/// Excel cache datetime (`2023-01-01` or `2023-01-01T00:00:00`) as a 1900 serial.
/// Returns nothing for text that is not that shape. Does not read a clock.
pub fn cache_datetime_serial(text: &str) -> Option<f64> {
    let text = text.trim().trim_end_matches('Z');
    if text.is_empty() {
        return None;
    }
    let (date, time) = text.split_once(['T', ' ']).unwrap_or((text, "0:0:0"));
    let mut parts = date.split('-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<i64>().ok()?;
    let day = parts.next()?.parse::<i64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let serial = serial_from_civil(year, month, day).ok()? as f64;
    let mut clock = time.split(':');
    let hour = clock.next()?.parse::<f64>().ok()?;
    let minute = clock.next().unwrap_or("0").parse::<f64>().ok()?;
    let second = clock.next().unwrap_or("0").parse::<f64>().ok()?;
    if !(0.0..24.0).contains(&hour)
        || !(0.0..60.0).contains(&minute)
        || !(0.0..61.0).contains(&second)
    {
        return None;
    }
    Some(serial + (hour * 3600.0 + minute * 60.0 + second) / 86400.0)
}

impl Workbook {
    /// Appends a pivot cache and returns its index. Install caches and tables
    /// before formulas that call `GETPIVOTDATA`; a later add does not recalculate.
    pub fn add_pivot_cache(&mut self, cache: PivotCache) -> usize {
        self.pivot_caches.push(cache);
        self.pivot_caches.len() - 1
    }

    /// Records a pivot table on `table.sheet`. The cache index is into
    /// [`Workbook::add_pivot_cache`] order.
    pub fn add_pivot_table(&mut self, table: PivotTable) {
        self.pivot_tables.push(table);
    }

    pub(super) fn get_pivot_data(&self, arguments: &[Expr<usize>]) -> Value {
        // Excel rejects a trailing field with no item as `#REF!`.
        if arguments.len() < 2 || arguments.len() % 2 != 0 {
            return Value::Error(CalcError::InvalidReference);
        }
        let data_name = match self.evaluate(&arguments[0]) {
            Value::Text(text) => text,
            Value::Error(error) => return Value::Error(error),
            _ => return Value::Error(CalcError::InvalidReference),
        };
        let Some((sheet, row0, column0, row1, column1)) = self.reference_rectangle(&arguments[1])
        else {
            return Value::Error(CalcError::InvalidReference);
        };
        let mut pairs = Vec::new();
        let mut index = 2;
        while index + 1 < arguments.len() {
            let field = match self.evaluate(&arguments[index]) {
                Value::Text(text) => text,
                Value::Error(error) => return Value::Error(error),
                _ => return Value::Error(CalcError::InvalidReference),
            };
            let item = match pivot_item(self.evaluate(&arguments[index + 1])) {
                Ok(item) => item,
                Err(error) => return Value::Error(error),
            };
            pairs.push((field, item));
            index += 2;
        }
        query_pivot(
            &self.pivot_caches,
            &self.pivot_tables,
            sheet,
            row0,
            column0,
            row1,
            column1,
            &data_name,
            &pairs,
        )
    }

    fn reference_rectangle(&self, expression: &Expr<usize>) -> Option<(u32, u32, u32, u32, u32)> {
        match expression {
            Expr::Reference(index) => {
                let id = self.cells[*index].id;
                Some((id.sheet, id.row, id.column, id.row, id.column))
            }
            Expr::RangeNode { node, .. } => match self.range_shape(*node) {
                RangeShape::Rectangle {
                    anchor,
                    rows,
                    columns,
                } => {
                    if rows == 0 || columns == 0 {
                        return None;
                    }
                    Some((
                        anchor.sheet,
                        anchor.row,
                        anchor.column,
                        anchor.row.saturating_add(rows as u32).saturating_sub(1),
                        anchor
                            .column
                            .saturating_add(columns as u32)
                            .saturating_sub(1),
                    ))
                }
                RangeShape::Members { .. } => {
                    let mut bounds: Option<(u32, u32, u32, u32, u32)> = None;
                    for member in &self.cells[*node].dependencies {
                        let id = self.cells[*member].id;
                        bounds = Some(match bounds {
                            None => (id.sheet, id.row, id.column, id.row, id.column),
                            Some((sheet, row0, column0, row1, column1)) if sheet == id.sheet => (
                                sheet,
                                row0.min(id.row),
                                column0.min(id.column),
                                row1.max(id.row),
                                column1.max(id.column),
                            ),
                            Some(existing) => existing,
                        });
                    }
                    bounds
                }
            },
            _ => None,
        }
    }
}

fn query_pivot(
    caches: &[PivotCache],
    tables: &[PivotTable],
    sheet: u32,
    row0: u32,
    column0: u32,
    row1: u32,
    column1: u32,
    data_name: &str,
    pairs: &[(String, PivotScalar)],
) -> Value {
    let Some(table) = find_table(tables, sheet, row0, column0, row1, column1) else {
        return Value::Error(CalcError::InvalidReference);
    };
    let Some(cache) = caches.get(table.cache) else {
        return Value::Error(CalcError::InvalidReference);
    };
    let Some(data) = find_data_field(table, data_name) else {
        return Value::Error(CalcError::InvalidReference);
    };
    if data.source >= cache.fields.len() {
        return Value::Error(CalcError::InvalidReference);
    }
    let mut constraints = Vec::with_capacity(pairs.len());
    for (name, item) in pairs {
        let Some(field) = find_axis_field(cache, table, name) else {
            return Value::Error(CalcError::InvalidReference);
        };
        if !item_in_view(cache, table, field, item) {
            return Value::Error(CalcError::InvalidReference);
        }
        constraints.push((field, item));
    }
    if data.aggregate != PivotAggregate::Sum {
        return Value::Error(CalcError::InvalidValue);
    }
    let mut sum = 0.0;
    for record in &cache.records {
        if !record_visible(cache, table, record) {
            continue;
        }
        if constraints
            .iter()
            .any(|(field, item)| !scalars_match(&field_value(cache, record, *field), item))
        {
            continue;
        }
        if let PivotScalar::Number(number) = field_value(cache, record, data.source) {
            if number.is_finite() {
                sum += number;
            }
        }
    }
    Value::Number(if sum == 0.0 { 0.0 } else { sum })
}

fn find_table<'a>(
    tables: &'a [PivotTable],
    sheet: u32,
    row0: u32,
    column0: u32,
    row1: u32,
    column1: u32,
) -> Option<&'a PivotTable> {
    let mut fallback = None;
    for table in tables {
        if table.sheet != sheet
            || row0 > table.last_row
            || table.first_row > row1
            || column0 > table.last_column
            || table.first_column > column1
        {
            continue;
        }
        let anchor_inside = row0 >= table.first_row
            && row0 <= table.last_row
            && column0 >= table.first_column
            && column0 <= table.last_column;
        if anchor_inside {
            return Some(table);
        }
        if fallback.is_none() {
            fallback = Some(table);
        }
    }
    fallback
}

fn find_data_field<'a>(table: &'a PivotTable, name: &str) -> Option<&'a PivotDataField> {
    let index = find_named(
        table
            .data_fields
            .iter()
            .enumerate()
            .map(|(index, field)| (index, field.name.as_str())),
        name,
    )?;
    table.data_fields.get(index)
}

fn find_axis_field(cache: &PivotCache, table: &PivotTable, name: &str) -> Option<usize> {
    find_named(
        table.axis_fields.iter().filter_map(|index| {
            cache
                .fields
                .get(*index)
                .map(|field| (*index, field.name.as_str()))
        }),
        name,
    )
}

/// Case-insensitive. A single leading or trailing space is ignored only when
/// the text itself does not match, so `" $ Inv Amount"` still matches itself.
fn find_named<'a>(names: impl Iterator<Item = (usize, &'a str)>, query: &str) -> Option<usize> {
    let names: Vec<(usize, &str)> = names.collect();
    if let Some((index, _)) = names
        .iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(query))
    {
        return Some(*index);
    }
    names
        .into_iter()
        .find(|(_, name)| fold_one_space(name).eq_ignore_ascii_case(fold_one_space(query)))
        .map(|(index, _)| index)
}

fn fold_one_space(text: &str) -> &str {
    let text = text.strip_prefix(' ').unwrap_or(text);
    text.strip_suffix(' ').unwrap_or(text)
}

fn item_in_view(cache: &PivotCache, table: &PivotTable, field: usize, item: &PivotScalar) -> bool {
    cache.records.iter().any(|record| {
        record_visible(cache, table, record)
            && scalars_match(&field_value(cache, record, field), item)
    })
}

fn record_visible(cache: &PivotCache, table: &PivotTable, record: &[PivotScalar]) -> bool {
    for (field, visible) in &table.visible_items {
        if !shared_visible(cache, record, *field, visible) {
            return false;
        }
    }
    for filter in &table.filters {
        // Excel does not apply a date filter whose field is no longer on the
        // pivot. The sample Period Date filter is off-axis (`evalOrder` -1)
        // and the cached totals include every record.
        if !table.axis_fields.contains(&filter.field) {
            continue;
        }
        let value = field_value(cache, record, filter.field);
        if !passes_date(&value, filter) {
            return false;
        }
    }
    true
}

fn shared_visible(
    cache: &PivotCache,
    record: &[PivotScalar],
    field: usize,
    visible: &[usize],
) -> bool {
    let value = field_value(cache, record, field);
    let Some(definition) = cache.fields.get(field) else {
        return false;
    };
    visible.iter().any(|index| {
        definition
            .shared
            .get(*index)
            .is_some_and(|item| scalars_match(item, &value))
    })
}

fn passes_date(value: &PivotScalar, filter: &PivotDateFilter) -> bool {
    let PivotScalar::Number(serial) = value else {
        return filter.negated;
    };
    if !serial.is_finite() {
        return filter.negated;
    }
    let low_ok = if filter.low_inclusive {
        *serial >= filter.low
    } else {
        *serial > filter.low
    };
    let high_ok = if filter.high_inclusive {
        *serial <= filter.high
    } else {
        *serial < filter.high
    };
    let inside = low_ok && high_ok;
    if filter.negated { !inside } else { inside }
}

fn field_value(cache: &PivotCache, record: &[PivotScalar], field: usize) -> PivotScalar {
    field_value_at(cache, record, field, 0)
}

fn field_value_at(
    cache: &PivotCache,
    record: &[PivotScalar],
    field: usize,
    depth: usize,
) -> PivotScalar {
    if depth > 4 {
        return PivotScalar::Blank;
    }
    let Some(definition) = cache.fields.get(field) else {
        return PivotScalar::Blank;
    };
    if let Some(group) = &definition.group {
        let PivotScalar::Number(serial) = field_value_at(cache, record, group.base, depth + 1)
        else {
            return PivotScalar::Blank;
        };
        return group_label(serial, group);
    }
    record.get(field).cloned().unwrap_or(PivotScalar::Blank)
}

fn group_label(serial: f64, group: &PivotDateGroup) -> PivotScalar {
    if !serial.is_finite() {
        return PivotScalar::Blank;
    }
    if serial < group.start {
        return text_item(group.items.first());
    }
    if serial > group.end {
        return text_item(group.items.last());
    }
    let Ok(civil) = civil_from_serial(serial.trunc() as i64) else {
        return PivotScalar::Blank;
    };
    if !(1..=12).contains(&civil.month) {
        return PivotScalar::Blank;
    }
    let generated = match group.by {
        PivotGroupBy::Years => civil.year.to_string(),
        PivotGroupBy::Quarters => format!("Qtr{}", (civil.month - 1) / 3 + 1),
        PivotGroupBy::Months => MONTHS[(civil.month as usize) - 1].to_string(),
    };
    let spelled = group
        .items
        .iter()
        .find(|item| item.eq_ignore_ascii_case(&generated))
        .cloned()
        .unwrap_or(generated);
    PivotScalar::Text(spelled)
}

fn text_item(item: Option<&String>) -> PivotScalar {
    match item {
        Some(text) => PivotScalar::Text(text.clone()),
        None => PivotScalar::Blank,
    }
}

fn pivot_item(value: Value) -> Result<PivotScalar, CalcError> {
    match value {
        Value::Blank => Ok(PivotScalar::Blank),
        Value::Number(number) if number.is_finite() => Ok(PivotScalar::Number(number)),
        Value::Number(_) => Err(CalcError::InvalidNumber),
        Value::Text(text) => Ok(PivotScalar::Text(text)),
        Value::Boolean(_) => Err(CalcError::InvalidReference),
        Value::Error(error) => Err(error),
    }
}

fn scalars_match(left: &PivotScalar, right: &PivotScalar) -> bool {
    match (left, right) {
        (PivotScalar::Blank, PivotScalar::Blank) => true,
        (PivotScalar::Number(left), PivotScalar::Number(right)) => numbers_match(*left, *right),
        (PivotScalar::Text(left), PivotScalar::Text(right)) => left.eq_ignore_ascii_case(right),
        (PivotScalar::Number(number), PivotScalar::Text(text))
        | (PivotScalar::Text(text), PivotScalar::Number(number)) => text
            .trim()
            .parse::<f64>()
            .ok()
            .is_some_and(|parsed| parsed.is_finite() && numbers_match(*number, parsed)),
        _ => false,
    }
}

fn numbers_match(left: f64, right: f64) -> bool {
    if left == right {
        return true;
    }
    let scale = left.abs().max(right.abs()).max(1.0);
    (left - right).abs() <= 1e-9 * scale
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial_date::serial_from_civil;

    fn number(value: f64) -> PivotScalar {
        PivotScalar::Number(value)
    }

    fn text(value: &str) -> PivotScalar {
        PivotScalar::Text(value.to_string())
    }

    fn sample() -> (PivotCache, f64, f64) {
        let jan1 = serial_from_civil(2023, 1, 1).unwrap() as f64;
        let feb1 = serial_from_civil(2023, 2, 1).unwrap() as f64;
        let cache = PivotCache {
            fields: vec![
                PivotCacheField {
                    name: "Region".into(),
                    shared: vec![text("East"), text("West")],
                    group: None,
                },
                PivotCacheField {
                    name: "Amount".into(),
                    shared: Vec::new(),
                    group: None,
                },
                PivotCacheField {
                    name: "When".into(),
                    shared: Vec::new(),
                    group: None,
                },
                PivotCacheField {
                    name: "Bonus".into(),
                    shared: Vec::new(),
                    group: None,
                },
                PivotCacheField {
                    name: "Years".into(),
                    shared: vec![text("<2023-01-01"), text("2023"), text(">2023-02-01")],
                    group: Some(PivotDateGroup {
                        base: 2,
                        by: PivotGroupBy::Years,
                        start: jan1,
                        end: feb1,
                        items: vec!["<2023-01-01".into(), "2023".into(), ">2023-02-01".into()],
                    }),
                },
            ],
            records: vec![
                vec![
                    text("East"),
                    number(10.0),
                    number(serial_from_civil(2023, 1, 15).unwrap() as f64),
                    PivotScalar::Blank,
                ],
                vec![
                    text("West"),
                    number(25.0),
                    number(serial_from_civil(2023, 2, 15).unwrap() as f64),
                    number(5.0),
                ],
                vec![
                    text("East"),
                    PivotScalar::Blank,
                    number(serial_from_civil(2023, 1, 20).unwrap() as f64),
                    PivotScalar::Blank,
                ],
                vec![text("East"), number(7.0), number(jan1), number(0.0)],
                vec![
                    text("East"),
                    number(3.0),
                    number(serial_from_civil(2023, 1, 31).unwrap() as f64),
                    PivotScalar::Blank,
                ],
                vec![
                    text("East"),
                    number(100.0),
                    number(feb1),
                    PivotScalar::Blank,
                ],
            ],
        };
        (cache, jan1, feb1)
    }

    fn table(
        cache: usize,
        column: u32,
        axis: Vec<usize>,
        filters: Vec<PivotDateFilter>,
        visible_items: Vec<(usize, Vec<usize>)>,
    ) -> PivotTable {
        PivotTable {
            sheet: 0,
            first_row: 0,
            last_row: 0,
            first_column: column,
            last_column: column,
            cache,
            data_fields: vec![
                PivotDataField {
                    name: " Amount".into(),
                    source: 1,
                    aggregate: PivotAggregate::Sum,
                },
                PivotDataField {
                    name: "Bonus".into(),
                    source: 3,
                    aggregate: PivotAggregate::Sum,
                },
                PivotDataField {
                    name: "Rows".into(),
                    source: 0,
                    aggregate: PivotAggregate::Unsupported,
                },
            ],
            axis_fields: axis,
            filters,
            visible_items,
        }
    }

    #[test]
    fn getpivotdata_sums_a_filtered_pivot_and_refuses_unknown_items() {
        let (cache, jan1, feb1) = sample();
        assert_eq!(jan1, 44_927.0);
        assert_eq!(serial_from_civil(2023, 1, 31).unwrap(), 44_957);
        assert_eq!(feb1, 44_958.0);
        let filter = PivotDateFilter {
            field: 2,
            low: jan1,
            high: serial_from_civil(2023, 1, 31).unwrap() as f64,
            low_inclusive: true,
            high_inclusive: true,
            negated: false,
        };
        let mut workbook = Workbook::default();
        let cache_index = workbook.add_pivot_cache(cache);
        workbook.add_pivot_table(table(
            cache_index,
            0,
            vec![0, 2],
            vec![filter.clone()],
            Vec::new(),
        ));
        workbook.add_pivot_table(table(cache_index, 2, vec![0, 4], vec![filter], Vec::new()));
        workbook.add_pivot_table(table(
            cache_index,
            4,
            vec![0],
            Vec::new(),
            vec![(0, vec![0])],
        ));

        let cases = [
            (
                1,
                1,
                r#"=GETPIVOTDATA(" Amount",$A$1)"#,
                Value::Number(20.0),
            ),
            (2, 1, r#"=GETPIVOTDATA("amount",$A$1)"#, Value::Number(20.0)),
            (
                3,
                1,
                r#"=GETPIVOTDATA(" Amount",$A$1,"region","East")"#,
                Value::Number(20.0),
            ),
            (
                4,
                1,
                r#"=GETPIVOTDATA(" Amount",$A$1,"Region","North")"#,
                Value::Error(CalcError::InvalidReference),
            ),
            (
                5,
                1,
                r#"=GETPIVOTDATA(" Amount",$A$1,"When",44927)"#,
                Value::Number(7.0),
            ),
            (
                6,
                1,
                r#"=GETPIVOTDATA(" Amount",$A$1,"Region")"#,
                Value::Error(CalcError::InvalidReference),
            ),
            (
                7,
                1,
                r#"=GETPIVOTDATA(" Amount",$Z$1)"#,
                Value::Error(CalcError::InvalidReference),
            ),
            (8, 1, r#"=GETPIVOTDATA("Bonus",$A$1)"#, Value::Number(0.0)),
            (
                9,
                1,
                r#"=GETPIVOTDATA("Rows",$A$1)"#,
                Value::Error(CalcError::InvalidValue),
            ),
            (
                10,
                1,
                r#"=GETPIVOTDATA(" Amount",$A$1,"Region","West")"#,
                Value::Error(CalcError::InvalidReference),
            ),
            (
                11,
                1,
                r#"=GETPIVOTDATA(" Amount",$A$1,"When",44958)"#,
                Value::Error(CalcError::InvalidReference),
            ),
            (
                12,
                1,
                r#"=_xlfn.GETPIVOTDATA(" Amount",$A$1,"Region","East")"#,
                Value::Number(20.0),
            ),
            (
                1,
                2,
                r#"=GETPIVOTDATA(" Amount",$C$1)"#,
                Value::Number(145.0),
            ),
            (
                2,
                2,
                r#"=GETPIVOTDATA(" Amount",$C$1,"Years","2023")"#,
                Value::Number(120.0),
            ),
            (
                3,
                2,
                r#"=GETPIVOTDATA(" Amount",$C$1,"Years",">2023-02-01")"#,
                Value::Number(25.0),
            ),
            (
                4,
                2,
                r#"=GETPIVOTDATA(" Amount",$C$1,"Years","2022")"#,
                Value::Error(CalcError::InvalidReference),
            ),
            (
                1,
                4,
                r#"=GETPIVOTDATA(" Amount",$E$1)"#,
                Value::Number(120.0),
            ),
            (
                2,
                4,
                r#"=GETPIVOTDATA(" Amount",$E$1,"Region","West")"#,
                Value::Error(CalcError::InvalidReference),
            ),
            (
                3,
                4,
                r#"=GETPIVOTDATA(" Amount",$E$1,"Region","East")"#,
                Value::Number(120.0),
            ),
        ];
        for (row, column, formula, expected) in cases {
            let cell = CellId::new(0, row, column);
            workbook.set_formula(cell, formula).unwrap();
            assert_eq!(workbook.value(cell), expected, "{formula}");
        }
    }
}
