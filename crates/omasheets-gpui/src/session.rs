//! Document session. GPUI types stay out of this module.

use crate::appearance::{AppearanceTile, VisibleWindow};
use omasheets_core::{
    Actor, ActorKind, ApplyError, CellInput, CellRef, CellValue, Command, Document, DocumentId,
    Literal, ObjectId, SheetId, column_letters,
};

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
        self.cell_ref(row, column)?;
        let address = CellAddress { row, column };
        self.selection = Some(address);
        self.formula_draft = self.input_text(row, column);
        Ok(address)
    }

    /// Commits the formula-bar draft into the selected cell.
    pub fn commit_edit(&mut self) -> Result<(CellAddress, String), ApplyError> {
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
        self.selection = None;
        self.formula_draft.clear();
        true
    }

    pub fn scroll_by(&mut self, rows: i32, columns: i32) {
        self.origin_row = offset(self.origin_row, rows);
        self.origin_column = offset(self.origin_column, columns);
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
        let Ok(cell) = self.cell_ref(row, column) else {
            return String::new();
        };
        display_value(&self.document.value(cell))
    }

    fn input_text(&self, row: usize, column: usize) -> String {
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

fn offset(origin: u32, delta: i32) -> u32 {
    if delta >= 0 {
        origin.saturating_add(delta as u32)
    } else {
        origin.saturating_sub(delta.unsigned_abs())
    }
}
