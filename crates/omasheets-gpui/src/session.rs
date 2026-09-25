//! Document session. GPUI types stay out of this module.

use crate::appearance::{AppearanceTile, SheetChrome, VisibleWindow};
use crate::browse::{self, BrowseBook};
use omasheets_core::{
    Actor, ActorKind, ApplyError, CellInput, CellRef, CellValue, Command, Document, DocumentId,
    Literal, ObjectId, SheetId, column_letters,
};
use std::collections::HashSet;
use std::path::Path;

pub const VISIBLE_ROWS: u32 = 32;
pub const VISIBLE_COLUMNS: u32 = 12;
const BACKING_ROWS: usize = 64;
const BACKING_COLUMNS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellAddress {
    pub row: usize,
    pub column: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VisibleCell {
    pub row: usize,
    pub column: usize,
    pub text: String,
    pub selected: bool,
    pub fill: Option<u32>,
    pub font: Option<u32>,
    pub merged: bool,
}

/// Applies commands to one document and projects the visible window.
pub struct SpreadsheetSession {
    document: Document,
    actor: Actor,
    clock: i64,
    active: usize,
    selection: Option<CellAddress>,
    origin_row: u32,
    origin_column: u32,
    formula_draft: String,
    appearance: Option<AppearanceTile>,
    browse: Option<BrowseBook>,
    chrome: Option<SheetChrome>,
}

impl SpreadsheetSession {
    /// Opens a document with one sheet large enough for the first viewport.
    pub fn open(name: impl Into<String>) -> Result<Self, ApplyError> {
        let name = name.into();
        let actor = Actor::new(ActorKind::Human, "host");
        let (mut document, _) = Document::create(
            DocumentId(ObjectId::from_seed(&name)),
            name,
            actor.clone(),
            1,
        )?;
        let mut clock = 1_i64;
        let event = document.command(
            actor.clone(),
            {
                clock += 1;
                clock
            },
            Command::AddSheet {
                name: "Sheet".into(),
            },
        )?;
        let omasheets_core::Operation::AddSheet { sheet, .. } = event.operation else {
            unreachable!("AddSheet resolves to AddSheet");
        };
        document.command(
            actor.clone(),
            {
                clock += 1;
                clock
            },
            Command::AddColumns {
                sheet,
                count: BACKING_COLUMNS,
                at: 0,
            },
        )?;
        document.command(
            actor.clone(),
            {
                clock += 1;
                clock
            },
            Command::AddRows {
                sheet,
                count: BACKING_ROWS,
                at: 0,
                table: None,
            },
        )?;
        Ok(Self {
            document,
            actor,
            clock,
            active: 0,
            selection: Some(CellAddress { row: 0, column: 0 }),
            origin_row: 0,
            origin_column: 0,
            formula_draft: String::new(),
            appearance: None,
            browse: None,
            chrome: None,
        })
    }

    /// Imports `path` and shows the owned engine's results.
    ///
    /// A formula the engine refused keeps the value stored in the file.
    /// The opened workbook is read-only: the formula bar shows the source,
    /// and committing an edit is rejected.
    pub fn open_xlsx(path: impl AsRef<Path>) -> Result<Self, crate::LoadError> {
        let path = path.as_ref();
        let book = browse::load(path)?;
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("Workbook");
        let mut session = Self::with_sheets(stem, &book)?;
        let start = book.first_occupied;
        session.browse = Some(book);
        // Cover sheets in these workbooks are often empty. Open on the first
        // sheet that actually has cells so the window is not a blank grid.
        if !session.activate_sheet(start) {
            let _ = session.select(0, 0);
        }
        Ok(session)
    }

    pub fn is_browsing(&self) -> bool {
        self.browse.is_some()
    }

    /// `Sheet · A1 · window A1`, or a blank-document label when nothing is open.
    pub fn place(&self) -> String {
        let sheet = self.active_sheet_name().unwrap_or("Sheet");
        let cell = self.selection_a1().unwrap_or_else(|| "A1".to_string());
        let origin = format!(
            "{}{}",
            column_letters(self.origin_column as usize),
            self.origin_row + 1
        );
        format!("{sheet} · {cell} · window {origin}")
    }

    /// One status line for a host: file, place, and how much was imported.
    pub fn summary(&self) -> String {
        let Some(book) = &self.browse else {
            return format!("{} · empty sheet", self.place());
        };
        let file = book
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workbook".to_string());
        format!(
            "{file} · {} · {} sheets · {} cells · {} formulas · engine values",
            self.place(),
            book.sheets.len(),
            book.occupied,
            book.formulas,
        )
    }

    pub(crate) fn workbook_path(&self) -> Option<&Path> {
        self.browse.as_ref().map(|book| book.path.as_path())
    }

    pub(crate) fn active_index(&self) -> usize {
        self.active
    }

    /// Package sheet name for the active tab, used to read styles.
    pub(crate) fn chrome_sheet_name(&self) -> Option<&str> {
        self.browse
            .as_ref()
            .and_then(|book| book.sheets.get(self.active))
            .map(|sheet| sheet.name.as_str())
    }

    pub(crate) fn install_chrome(&mut self, sheet_index: usize, chrome: Option<SheetChrome>) {
        if sheet_index != self.active {
            return;
        }
        self.chrome = chrome;
        if self.chrome.is_some() {
            self.retile();
        } else {
            self.appearance = None;
        }
    }

    pub(crate) fn retile(&mut self) {
        if let Some(chrome) = &self.chrome {
            let window = self.visible_window();
            self.appearance = Some(chrome.tile(window));
        }
    }

    fn with_sheets(name: &str, book: &BrowseBook) -> Result<Self, crate::LoadError> {
        let actor = Actor::new(ActorKind::Human, "host");
        let (mut document, _) = Document::create(
            DocumentId(ObjectId::from_seed(name)),
            document_name(name),
            actor.clone(),
            1,
        )
        .map_err(crate::LoadError::Document)?;
        let mut clock = 1_i64;
        let mut used = HashSet::new();
        for (index, sheet) in book.sheets.iter().enumerate() {
            clock += 1;
            document
                .command(
                    actor.clone(),
                    clock,
                    Command::AddSheet {
                        name: unique_sheet_name(&sheet.name, index, &mut used),
                    },
                )
                .map_err(crate::LoadError::Document)?;
        }
        Ok(Self {
            document,
            actor,
            clock,
            active: 0,
            selection: None,
            origin_row: 0,
            origin_column: 0,
            formula_draft: String::new(),
            appearance: None,
            browse: None,
            chrome: None,
        })
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn sheet_names(&self) -> Vec<String> {
        self.document
            .sheets()
            .iter()
            .filter_map(|sheet| self.document.sheet_name(*sheet).map(str::to_string))
            .collect()
    }

    pub fn active_sheet_name(&self) -> Option<&str> {
        self.active_sheet()
            .and_then(|sheet| self.document.sheet_name(sheet))
    }

    pub fn selection(&self) -> Option<CellAddress> {
        self.selection
    }

    pub fn selection_a1(&self) -> Option<String> {
        self.selection
            .map(|address| format!("{}{}", column_letters(address.column), address.row + 1))
    }

    pub fn formula_draft(&self) -> &str {
        &self.formula_draft
    }

    pub fn set_formula_draft(&mut self, text: String) {
        self.formula_draft = text;
    }

    pub fn appearance(&self) -> Option<&AppearanceTile> {
        self.appearance.as_ref()
    }

    pub fn set_appearance(&mut self, tile: AppearanceTile) {
        self.appearance = Some(tile);
    }

    pub fn visible_window(&self) -> VisibleWindow {
        VisibleWindow {
            origin_row: self.origin_row,
            origin_column: self.origin_column,
            rows: VISIBLE_ROWS,
            columns: VISIBLE_COLUMNS,
        }
    }

    pub fn column_labels(&self) -> Vec<String> {
        (0..VISIBLE_COLUMNS)
            .map(|offset| column_letters(self.origin_column as usize + offset as usize))
            .collect()
    }

    pub fn row_labels(&self) -> Vec<String> {
        (0..VISIBLE_ROWS)
            .map(|offset| (self.origin_row as usize + offset as usize + 1).to_string())
            .collect()
    }

    /// Selects a cell in the current view. The address is absolute, not an offset.
    pub fn select(&mut self, row: usize, column: usize) -> Result<CellAddress, ApplyError> {
        let (rows, columns) = self.sheet_bounds();
        if row >= rows || column >= columns {
            return Err(ApplyError::ReferenceOutOfView(format!(
                "{}{}",
                column_letters(column),
                row + 1
            )));
        }
        if self.browse.is_none() {
            self.cell_ref(row, column)?;
        }
        let address = CellAddress { row, column };
        self.selection = Some(address);
        self.formula_draft = self.input_text(row, column);
        Ok(address)
    }

    /// Moves the selection by `row_delta` and `column_delta`, scrolling the
    /// window when the cell would leave it. Deltas are clamped to the sheet.
    pub fn move_selection(
        &mut self,
        row_delta: i32,
        column_delta: i32,
    ) -> Result<CellAddress, ApplyError> {
        let (rows, columns) = self.sheet_bounds();
        let current = self.selection.unwrap_or(CellAddress { row: 0, column: 0 });
        let row = shift_index(current.row, row_delta, rows);
        let column = shift_index(current.column, column_delta, columns);
        self.reveal(row, column);
        self.select(row, column)
    }

    /// Commits the formula-bar draft into the selected cell.
    pub fn commit_edit(&mut self) -> Result<(CellAddress, String), ApplyError> {
        if self.browse.is_some() {
            return Err(ApplyError::ReferenceOutOfView(
                "opened workbooks are read-only in this view".into(),
            ));
        }
        let address = self
            .selection
            .ok_or(ApplyError::ReferenceOutOfView("no cell is selected".into()))?;
        let sheet =
            self.active_sheet()
                .ok_or(ApplyError::UnknownSheet(SheetId(ObjectId::from_seed(
                    "missing",
                ))))?;
        let a1 = format!("{}{}", column_letters(address.column), address.row + 1);
        let source = self.formula_draft.clone();
        let command = cell_command(sheet, a1, &source);
        self.apply_command(command)?;
        self.formula_draft = source.clone();
        Ok((address, source))
    }

    /// Host entry: resolve and apply one command. A rejection leaves the document unchanged.
    pub fn apply_command(&mut self, command: Command) -> Result<(), ApplyError> {
        self.clock += 1;
        self.document
            .command(self.actor.clone(), self.clock, command)?;
        self.clamp_view();
        Ok(())
    }

    pub fn activate_sheet(&mut self, index: usize) -> bool {
        if index >= self.document.sheets().len() {
            return false;
        }
        self.active = index;
        self.origin_row = 0;
        self.origin_column = 0;
        self.chrome = None;
        self.appearance = None;
        self.selection = None;
        self.formula_draft.clear();
        let _ = self.select(0, 0);
        true
    }

    pub fn scroll_by(&mut self, rows: i32, columns: i32) {
        self.origin_row = offset(self.origin_row, rows);
        self.origin_column = offset(self.origin_column, columns);
        self.clamp_origin();
    }

    pub fn visible_cells(&self) -> Vec<VisibleCell> {
        let mut cells = Vec::with_capacity((VISIBLE_ROWS * VISIBLE_COLUMNS) as usize);
        for row_offset in 0..VISIBLE_ROWS {
            for column_offset in 0..VISIBLE_COLUMNS {
                let row = self.origin_row as usize + row_offset as usize;
                let column = self.origin_column as usize + column_offset as usize;
                let selected = self.selection == Some(CellAddress { row, column });
                let text = self.display_text(row, column);
                let (fill, font, merged) = self.paint_facts(row as u32, column as u32);
                cells.push(VisibleCell {
                    row,
                    column,
                    text,
                    selected,
                    fill,
                    font,
                    merged,
                });
            }
        }
        cells
    }

    pub fn column_width_px(&self, column: usize) -> f32 {
        let width = self
            .appearance
            .as_ref()
            .and_then(|tile| tile.width(column as u32))
            .unwrap_or(9.0);
        (width as f32 * 8.0).clamp(28.0, 240.0)
    }

    fn paint_facts(&self, row: u32, column: u32) -> (Option<u32>, Option<u32>, bool) {
        let Some(tile) = &self.appearance else {
            return (None, None, false);
        };
        (
            tile.fill(row, column).map(crate::RgbColor::packed),
            tile.font(row, column).map(crate::RgbColor::packed),
            tile.merge_at(row, column).is_some(),
        )
    }

    fn display_text(&self, row: usize, column: usize) -> String {
        if self.browse.is_some() {
            return self
                .browse_cell(row, column)
                .map(|cell| cell.text.clone())
                .unwrap_or_default();
        }
        let Ok(cell) = self.cell_ref(row, column) else {
            return String::new();
        };
        display_value(&self.document.value(cell))
    }

    fn input_text(&self, row: usize, column: usize) -> String {
        if let Some(cell) = self.browse_cell(row, column) {
            return cell.input.clone();
        }
        if self.browse.is_some() {
            return String::new();
        }
        let Ok(cell) = self.cell_ref(row, column) else {
            return String::new();
        };
        match self.document.cell(cell).map(|state| &state.input) {
            Some(CellInput::Formula { formula }) => formula.source.clone(),
            Some(CellInput::Value { value }) => literal_text(value),
            None => String::new(),
        }
    }

    fn cell_ref(&self, row: usize, column: usize) -> Result<CellRef, ApplyError> {
        let sheet = self
            .active_sheet()
            .ok_or_else(|| ApplyError::ReferenceOutOfView("the document has no sheet".into()))?;
        let rows = self
            .document
            .rows(sheet)
            .ok_or_else(|| ApplyError::ReferenceOutOfView(format!("row {}", row + 1)))?;
        let columns = self
            .document
            .columns(sheet)
            .ok_or_else(|| ApplyError::ReferenceOutOfView(column_letters(column)))?;
        let row_id = *rows.get(row).ok_or_else(|| {
            ApplyError::ReferenceOutOfView(format!("{}{}", column_letters(column), row + 1))
        })?;
        let column_id = *columns.get(column).ok_or_else(|| {
            ApplyError::ReferenceOutOfView(format!("{}{}", column_letters(column), row + 1))
        })?;
        Ok(CellRef {
            sheet,
            row: row_id,
            column: column_id,
        })
    }

    fn active_sheet(&self) -> Option<SheetId> {
        self.document.sheets().get(self.active).copied()
    }

    fn browse_cell(&self, row: usize, column: usize) -> Option<&browse::BrowseCell> {
        let book = self.browse.as_ref()?;
        let sheet = book.sheets.get(self.active)?;
        let row = u32::try_from(row).ok()?;
        let column = u32::try_from(column).ok()?;
        book.cells.get(&(sheet.index, row, column))
    }

    fn sheet_bounds(&self) -> (usize, usize) {
        if let Some(book) = &self.browse {
            if let Some(sheet) = book.sheets.get(self.active) {
                return (
                    usize::try_from(sheet.rows).unwrap_or(usize::MAX).max(1),
                    usize::try_from(sheet.columns).unwrap_or(usize::MAX).max(1),
                );
            }
        }
        let Some(sheet) = self.active_sheet() else {
            return (1, 1);
        };
        let rows = self
            .document
            .rows(sheet)
            .map(|rows| rows.len())
            .unwrap_or(0)
            .max(1);
        let columns = self
            .document
            .columns(sheet)
            .map(|columns| columns.len())
            .unwrap_or(0)
            .max(1);
        (rows, columns)
    }

    fn reveal(&mut self, row: usize, column: usize) {
        let row = u32::try_from(row).unwrap_or(u32::MAX);
        let column = u32::try_from(column).unwrap_or(u32::MAX);
        if row < self.origin_row {
            self.origin_row = row;
        } else if row >= self.origin_row.saturating_add(VISIBLE_ROWS) {
            self.origin_row = row + 1 - VISIBLE_ROWS;
        }
        if column < self.origin_column {
            self.origin_column = column;
        } else if column >= self.origin_column.saturating_add(VISIBLE_COLUMNS) {
            self.origin_column = column + 1 - VISIBLE_COLUMNS;
        }
        self.clamp_origin();
    }

    fn clamp_origin(&mut self) {
        let (rows, columns) = self.sheet_bounds();
        let max_row = u32::try_from(rows.saturating_sub(VISIBLE_ROWS as usize)).unwrap_or(u32::MAX);
        let max_column =
            u32::try_from(columns.saturating_sub(VISIBLE_COLUMNS as usize)).unwrap_or(u32::MAX);
        self.origin_row = self.origin_row.min(max_row);
        self.origin_column = self.origin_column.min(max_column);
    }

    fn clamp_view(&mut self) {
        let count = self.document.sheets().len();
        if count == 0 {
            self.active = 0;
            self.selection = None;
            self.formula_draft.clear();
            return;
        }
        if self.active >= count {
            self.active = count - 1;
            self.selection = None;
            self.formula_draft.clear();
        }
        if let Some(address) = self.selection {
            if self.cell_ref(address.row, address.column).is_err() {
                self.selection = None;
                self.formula_draft.clear();
            }
        }
    }
}

fn cell_command(sheet: SheetId, a1: String, source: &str) -> Command {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Command::ClearCell { sheet, a1 };
    }
    if trimmed.starts_with('=') {
        return Command::SetFormula {
            sheet,
            a1,
            source: trimmed.to_string(),
        };
    }
    if trimmed.eq_ignore_ascii_case("true") {
        return Command::SetValue {
            sheet,
            a1,
            value: Literal::Boolean(true),
        };
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return Command::SetValue {
            sheet,
            a1,
            value: Literal::Boolean(false),
        };
    }
    if let Ok(number) = trimmed.parse::<f64>() {
        if number.is_finite() {
            return Command::SetValue {
                sheet,
                a1,
                value: Literal::Number(number),
            };
        }
    }
    Command::SetValue {
        sheet,
        a1,
        value: Literal::Text(trimmed.to_string()),
    }
}

fn literal_text(value: &Literal) -> String {
    match value {
        Literal::Blank => String::new(),
        Literal::Number(number) => format_number(*number),
        Literal::Text(text) => text.clone(),
        Literal::Boolean(flag) => {
            if *flag {
                "TRUE".into()
            } else {
                "FALSE".into()
            }
        }
    }
}

fn display_value(value: &CellValue) -> String {
    match value {
        CellValue::Blank => String::new(),
        CellValue::Number(number) => format_number(*number),
        CellValue::Text(text) => text.clone(),
        CellValue::Boolean(flag) => {
            if *flag {
                "TRUE".into()
            } else {
                "FALSE".into()
            }
        }
        CellValue::Error(label) => label.clone(),
    }
}

fn format_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_53647: &str = "/Users/markwatts/omasheets-corpus/spreadsheet-rl-2026/sample/spreadsheetbench_verified__spreadsheet__1_53647__input.xlsx";
    const ROLLUP: &str = "/Users/markwatts/omasheets-corpus/spreadsheet-rl-2026/sample/spreadsheetbench_2__Debugging__10_01__input.xlsx";

    #[test]
    fn arrow_movement_stays_inside_the_backing_sheet() {
        let mut session = SpreadsheetSession::open("book").expect("session");
        let address = session.move_selection(10_000, 10_000).expect("move");
        assert_eq!(address.row, BACKING_ROWS - 1);
        assert_eq!(address.column, BACKING_COLUMNS - 1);
        let origin_before = (session.origin_row, session.origin_column);
        session.scroll_by(10_000, 10_000);
        assert!(session.origin_row <= origin_before.0.saturating_add(BACKING_ROWS as u32));
        assert_eq!(
            session.origin_row,
            (BACKING_ROWS as u32).saturating_sub(VISIBLE_ROWS)
        );
        assert_eq!(
            session.origin_column,
            (BACKING_COLUMNS as u32).saturating_sub(VISIBLE_COLUMNS)
        );
    }

    #[test]
    fn opens_small_sample_when_present() {
        let path = Path::new(SAMPLE_53647);
        if !path.is_file() {
            eprintln!("skipping; sample workbook is absent");
            return;
        }
        let mut session = SpreadsheetSession::open_xlsx(path).expect("open sample");
        assert!(session.is_browsing());
        assert!(!session.sheet_names().is_empty());
        let summary = session.summary();
        assert!(summary.contains("engine values"), "{summary}");
        assert!(!summary.contains(" 0 cells"), "{summary}");
        let nonempty = session
            .visible_cells()
            .iter()
            .filter(|cell| !cell.text.is_empty())
            .count();
        assert!(nonempty > 0, "{summary}");
        let moved = session.move_selection(1, 1).expect("move");
        assert_eq!((moved.row, moved.column), (1, 1));
        assert!(session.visible_cells().iter().any(|cell| cell.selected));
    }

    #[test]
    fn opens_operating_rollup_when_requested() {
        if std::env::var_os("OMASHEETS_OPEN_COMPLEX").is_none() {
            return;
        }
        let path = Path::new(ROLLUP);
        assert!(path.is_file(), "corpus rollup is not on disk");
        let started = std::time::Instant::now();
        let session = SpreadsheetSession::open_xlsx(path).expect("open rollup");
        eprintln!(
            "opened {} in {:?} · {}",
            path.display(),
            started.elapsed(),
            session.summary()
        );
        assert!(session.sheet_names().len() > 10);
        assert!(session.summary().contains("formulas"));
    }
}

fn offset(origin: u32, delta: i32) -> u32 {
    if delta >= 0 {
        origin.saturating_add(delta as u32)
    } else {
        origin.saturating_sub(delta.unsigned_abs())
    }
}

fn shift_index(index: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let last = len - 1;
    if delta >= 0 {
        index.saturating_add(delta as usize).min(last)
    } else {
        index
            .saturating_sub(delta.unsigned_abs() as usize)
            .min(last)
    }
}

fn document_name(name: &str) -> String {
    if valid_label(name) {
        name.to_string()
    } else {
        "Workbook".to_string()
    }
}

fn unique_sheet_name(raw: &str, index: usize, used: &mut HashSet<String>) -> String {
    let base = if valid_label(raw) {
        raw.to_string()
    } else {
        format!("Sheet {}", index + 1)
    };
    if used.insert(base.clone()) {
        return base;
    }
    let mut suffix = 2_u32;
    loop {
        let candidate = format!("{base} ({suffix})");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

fn valid_label(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && name.chars().count() <= 255
        && !name.chars().any(char::is_control)
}
