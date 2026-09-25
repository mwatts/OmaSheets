//! Display grid for one imported workbook.
//!
//! The event document keeps a sheet tab per worksheet. Cell text is projected
//! here so a corpus workbook can be shown without replaying every formula as
//! a document command. A formula the owned engine compiled shows that result.
//! A formula it refused keeps the stored cache.

use crate::format::{self, paint_number};
use omasheets_calc::{CellId, Value};
use omasheets_core::ApplyError;
use omasheets_xlsx::{ImportError, ImportLimits, import_xlsx};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// Why [`crate::SpreadsheetSession::open_xlsx`] could not open a workbook.
#[derive(Debug)]
pub enum LoadError {
    Import(ImportError),
    Document(ApplyError),
    NoSheets,
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Import(error) => write!(formatter, "{error}"),
            Self::Document(error) => write!(formatter, "{error}"),
            Self::NoSheets => write!(formatter, "workbook has no worksheets"),
        }
    }
}

impl std::error::Error for LoadError {}

pub(crate) struct BrowseCell {
    pub(crate) text: String,
    pub(crate) input: String,
    /// True when the cell value is a number, so the grid right-aligns it.
    pub(crate) numeric: bool,
    /// `0xRRGGBB` from the number format, such as `[Red]`.
    pub(crate) format_color: Option<u32>,
}

pub(crate) struct BrowseSheet {
    /// Worksheet name as stored in the package, used to read styles.
    pub(crate) name: String,
    pub(crate) index: u32,
    pub(crate) rows: u32,
    pub(crate) columns: u32,
}

pub(crate) struct BrowseBook {
    pub(crate) path: PathBuf,
    pub(crate) sheets: Vec<BrowseSheet>,
    pub(crate) cells: HashMap<(u32, u32, u32), BrowseCell>,
    /// Custom row heights in points, keyed by engine sheet index and 0-based row.
    pub(crate) row_points: HashMap<(u32, u32), f64>,
    pub(crate) occupied: usize,
    pub(crate) formulas: usize,
    /// Index into `sheets` of the first worksheet that has a cell.
    /// Cover sheets in the corpus are often empty.
    pub(crate) first_occupied: usize,
}

pub(crate) fn load(path: &Path) -> Result<BrowseBook, LoadError> {
    let imported = import_xlsx(path, ImportLimits::default()).map_err(LoadError::Import)?;
    if imported.sheets.is_empty() {
        return Err(LoadError::NoSheets);
    }
    let stored_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let look = format::read_workbook_look(path);
    let names: HashMap<u32, String> = imported
        .sheets
        .iter()
        .map(|sheet| (sheet.index, sheet.name.clone()))
        .collect();
    let refused: HashSet<CellId> = imported.unsupported.iter().map(|item| item.cell).collect();
    let mut cells = HashMap::with_capacity(imported.source_cells().len());
    let mut formulas = 0_usize;
    for source in imported.source_cells() {
        let value = if source.formula.is_some() && !refused.contains(&source.cell) {
            imported.workbook.value(source.cell)
        } else {
            source.stored.clone()
        };
        let sheet_name = names
            .get(&source.cell.sheet)
            .map(String::as_str)
            .unwrap_or("");
        let code = look
            .formats
            .get(&(sheet_name.to_string(), source.cell.row, source.cell.column))
            .map(String::as_str)
            .unwrap_or("");
        let (text, numeric, format_color) = display_value(&value, code);
        let input = match &source.formula {
            Some(formula) => {
                formulas += 1;
                formula_source(formula)
            }
            None => text.clone(),
        };
        cells.insert(
            (source.cell.sheet, source.cell.row, source.cell.column),
            BrowseCell {
                text,
                input,
                numeric,
                format_color,
            },
        );
    }
    let mut row_points = HashMap::new();
    for ((name, row), points) in look.row_points {
        let Some(index) = imported
            .sheets
            .iter()
            .find(|sheet| sheet.name == name)
            .map(|sheet| sheet.index)
        else {
            continue;
        };
        row_points.insert((index, row), points);
    }
    let occupied = cells.len();
    let first_sheet = imported
        .source_cells()
        .first()
        .map(|cell| cell.cell.sheet)
        .unwrap_or(0);
    let sheets: Vec<BrowseSheet> = imported
        .sheets
        .iter()
        .map(|sheet| BrowseSheet {
            name: sheet.name.clone(),
            index: sheet.index,
            rows: u32::try_from(sheet.rows).unwrap_or(u32::MAX),
            columns: u32::try_from(sheet.columns).unwrap_or(u32::MAX),
        })
        .collect();
    let first_occupied = sheets
        .iter()
        .position(|sheet| sheet.index == first_sheet)
        .unwrap_or(0);
    Ok(BrowseBook {
        path: stored_path,
        sheets,
        cells,
        row_points,
        occupied,
        formulas,
        first_occupied,
    })
}

fn formula_source(formula: &str) -> String {
    let trimmed = formula.trim();
    if trimmed.starts_with('=') {
        trimmed.to_string()
    } else {
        format!("={trimmed}")
    }
}

fn display_value(value: &Value, code: &str) -> (String, bool, Option<u32>) {
    match value {
        Value::Blank => (String::new(), false, None),
        Value::Number(number) => {
            let painted = paint_number(*number, code);
            (painted.text, true, painted.color)
        }
        Value::Text(text) => (text.clone(), false, None),
        Value::Boolean(true) => ("TRUE".to_string(), false, None),
        Value::Boolean(false) => ("FALSE".to_string(), false, None),
        Value::Error(error) => (error.label().to_string(), false, None),
    }
}
