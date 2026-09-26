//! Document session. GPUI types stay out of this module.

use crate::appearance::{AppearanceTile, SheetChrome, VisibleWindow};
use crate::browse::{self, BrowseBook};
use crate::media::{is_native_media_type, sniff_spreadsheet_media_type, XLSX_MEDIA_TYPE};
use omasheets_core::{
    column_letters, Actor, ActorKind, ApplyError, BranchId, CellInput, CellRef, CellValue, Command,
    Document, DocumentId, Literal, ObjectId, SheetId,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const VISIBLE_ROWS: u32 = 32;
pub const VISIBLE_COLUMNS: u32 = 12;
const BACKING_ROWS: usize = 64;
const BACKING_COLUMNS: usize = 16;

/// On-disk native store the session appends into for durable commits.
struct NativeDurable {
    path: PathBuf,
    branch: BranchId,
}

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
    /// A number is right-aligned. Text, booleans, and errors are left-aligned.
    pub numeric: bool,
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
    /// Pixel overrides keyed by the active sheet index and the column or row.
    column_px: HashMap<(usize, usize), f32>,
    row_px: HashMap<(usize, usize), f32>,
    /// When set, formula-bar commands append into this native store.
    native: Option<NativeDurable>,
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
            column_px: HashMap::new(),
            row_px: HashMap::new(),
            native: None,
        })
    }

    /// Imports `path` and shows the owned engine's results.
    ///
    /// A formula the engine refused keeps the value stored in the file.
    /// Committing the formula bar writes into the calculation engine and
    /// refreshes displayed dependents. The file on disk is left unchanged.
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

    /// Imports workbook bytes through a private temp package, then opens it.
    ///
    /// The temp file is kept so sheet chrome and appearance can still read the
    /// package. Callers that own a durable store (Ashlar's workbook port) should
    /// treat this as a hydrate cache, not the publication path.
    pub fn open_xlsx_bytes(
        bytes: impl AsRef<[u8]>,
        label: impl Into<String>,
    ) -> Result<Self, crate::LoadError> {
        let label = label.into();
        let path = Self::write_temp_bytes(&label, "xlsx", bytes.as_ref())?;
        Self::open_xlsx(path)
    }

    /// Opens a native `.omasheets` store and shows its main-branch document.
    ///
    /// Formula-bar commands append into this store so [`Self::durable_bytes`]
    /// can checkpoint and export native file bytes.
    pub fn open_omasheets(path: impl AsRef<Path>) -> Result<Self, crate::LoadError> {
        let path = path.as_ref().to_path_buf();
        let mut store = omasheets_store::Store::open(&path).map_err(crate::LoadError::Store)?;
        let branch = store.branch_id("main").map_err(crate::LoadError::Store)?;
        let document = store
            .document(branch)
            .map_err(crate::LoadError::Store)?
            .clone();
        if document.sheets().is_empty() {
            return Err(crate::LoadError::NoSheets);
        }
        let mut session = Self::from_document(document);
        session.native = Some(NativeDurable { path, branch });
        Ok(session)
    }

    /// Writes native store bytes to a temp `.omasheets` file and opens them.
    pub fn open_omasheets_bytes(
        bytes: impl AsRef<[u8]>,
        label: impl Into<String>,
    ) -> Result<Self, crate::LoadError> {
        let label = label.into();
        let path = Self::write_temp_bytes(&label, "omasheets", bytes.as_ref())?;
        Self::open_omasheets(path)
    }

    /// Opens workbook bytes using the Ashlar / Freedesktop media type.
    ///
    /// Native documents use [`crate::NATIVE_MEDIA_TYPE`]. OOXML packages use the
    /// Excel spreadsheet media type and remain import-only interchange. Empty or
    /// `application/octet-stream` types are sniffed from the leading bytes.
    pub fn open_bytes(
        bytes: impl AsRef<[u8]>,
        label: impl Into<String>,
        content_type: &str,
    ) -> Result<Self, crate::LoadError> {
        let label = label.into();
        let bytes = bytes.as_ref();
        if is_native_media_type(content_type) {
            return Self::open_omasheets_bytes(bytes, label);
        }
        let essence = content_type
            .split(';')
            .next()
            .unwrap_or(content_type)
            .trim();
        if essence.is_empty() || essence.eq_ignore_ascii_case("application/octet-stream") {
            return match sniff_spreadsheet_media_type(bytes) {
                Some(crate::NATIVE_MEDIA_TYPE) => Self::open_omasheets_bytes(bytes, label),
                Some(crate::XLSX_MEDIA_TYPE) => Self::open_xlsx_bytes(bytes, label),
                _ => Err(crate::LoadError::UnsupportedMediaType(
                    content_type.to_owned(),
                )),
            };
        }
        if essence.eq_ignore_ascii_case(XLSX_MEDIA_TYPE)
            || essence.eq_ignore_ascii_case("application/vnd.ms-excel")
        {
            return Self::open_xlsx_bytes(bytes, label);
        }
        Err(crate::LoadError::UnsupportedMediaType(
            content_type.to_owned(),
        ))
    }

    fn write_temp_bytes(
        label: &str,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, crate::LoadError> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let safe: String = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .take(48)
            .collect();
        let path = std::env::temp_dir().join(format!(
            "omasheets-bytes-{safe}-{}-{nonce}.{extension}",
            std::process::id()
        ));
        std::fs::write(&path, bytes).map_err(crate::LoadError::Io)?;
        Ok(path)
    }

    fn from_document(document: Document) -> Self {
        let clock = i64::try_from(document.event_count()).unwrap_or(i64::MAX);
        Self {
            document,
            actor: Actor::new(ActorKind::Human, "host"),
            clock,
            active: 0,
            selection: Some(CellAddress { row: 0, column: 0 }),
            origin_row: 0,
            origin_column: 0,
            formula_draft: String::new(),
            appearance: None,
            browse: None,
            chrome: None,
            column_px: HashMap::new(),
            row_px: HashMap::new(),
            native: None,
        }
    }

    pub fn is_browsing(&self) -> bool {
        self.browse.is_some()
    }

    /// True when formula commits append into a native `.omasheets` store.
    #[must_use]
    pub fn is_native_durable(&self) -> bool {
        self.native.is_some()
    }

    /// Checkpoint a native store and return its bytes for a workbook commit.
    ///
    /// Returns `Ok(None)` for xlsx browse sessions (no durable export yet).
    pub fn durable_bytes(&mut self) -> Result<Option<Vec<u8>>, crate::LoadError> {
        let Some(native) = &self.native else {
            return Ok(None);
        };
        let path = native.path.clone();
        let store = omasheets_store::Store::open(&path).map_err(crate::LoadError::Store)?;
        store.close().map_err(crate::LoadError::Store)?;
        let bytes = std::fs::read(&path).map_err(crate::LoadError::Io)?;
        Ok(Some(bytes))
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
            column_px: HashMap::new(),
            row_px: HashMap::new(),
            native: None,
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
    ///
    /// An opened workbook writes through the calculation engine and refreshes
    /// the displayed value, including formulas that depend on the edit.
    pub fn commit_edit(&mut self) -> Result<(CellAddress, String), ApplyError> {
        let address = self
            .selection
            .ok_or(ApplyError::ReferenceOutOfView("no cell is selected".into()))?;
        if self.browse.is_some() {
            let sheet = self
                .browse
                .as_ref()
                .and_then(|book| book.sheets.get(self.active))
                .map(|sheet| sheet.index)
                .ok_or_else(|| {
                    ApplyError::ReferenceOutOfView("the opened workbook has no sheet".into())
                })?;
            let source = self.formula_draft.clone();
            self.browse.as_mut().expect("browse checked").apply_input(
                sheet,
                address.row as u32,
                address.column as u32,
                &source,
            )?;
            return Ok((address, source));
        }
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
        if let Some(native) = self.native.as_ref() {
            let path = native.path.clone();
            let branch = native.branch;
            self.document = persist_native(&path, branch, self.actor.clone(), self.clock, command)?;
            self.clamp_view();
            Ok(())
        } else {
            self.document
                .command(self.actor.clone(), self.clock, command)?;
            self.clamp_view();
            Ok(())
        }
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
                let (text, numeric, format_color) = self.display_cell(row, column);
                let (fill, mut font, merged) = self.paint_facts(row as u32, column as u32);
                if let Some(color) = format_color {
                    font = Some(color);
                }
                cells.push(VisibleCell {
                    row,
                    column,
                    text,
                    numeric,
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
        if let Some(width) = self.column_px.get(&(self.active, column)) {
            return *width;
        }
        let width = self
            .appearance
            .as_ref()
            .and_then(|tile| tile.width(column as u32))
            .unwrap_or(9.0);
        (width as f32 * 8.0).clamp(24.0, 2000.0)
    }

    pub fn row_height_px(&self, row: usize) -> f32 {
        if let Some(height) = self.row_px.get(&(self.active, row)) {
            return *height;
        }
        let points = self.row_points(row).unwrap_or(15.0);
        (points as f32 * (22.0 / 15.0)).clamp(12.0, 400.0)
    }

    pub fn set_column_width_px(&mut self, column: usize, width: f32) {
        self.column_px
            .insert((self.active, column), width.clamp(24.0, 2000.0));
    }

    pub fn set_row_height_px(&mut self, row: usize, height: f32) {
        self.row_px
            .insert((self.active, row), height.clamp(12.0, 400.0));
    }

    /// Width from the longest displayed value in the column, 8 px per character plus padding.
    pub fn autofit_column(&mut self, column: usize) {
        let width = self.longest_text(Some(column), None) as f32 * 8.0 + 16.0;
        self.set_column_width_px(column, width);
    }

    /// Unwrapped cells are one line. A double-click restores that line height.
    pub fn autofit_row(&mut self, row: usize) {
        self.set_row_height_px(row, 22.0);
    }

    fn row_points(&self, row: usize) -> Option<f64> {
        let book = self.browse.as_ref()?;
        let sheet = book.sheets.get(self.active)?;
        let row = u32::try_from(row).ok()?;
        book.row_points.get(&(sheet.index, row)).copied()
    }

    fn longest_text(&self, column: Option<usize>, row: Option<usize>) -> usize {
        if let Some(book) = &self.browse {
            let Some(sheet) = book.sheets.get(self.active) else {
                return 0;
            };
            return book
                .cells
                .iter()
                .filter_map(|((sheet_index, cell_row, cell_column), cell)| {
                    if *sheet_index != sheet.index {
                        return None;
                    }
                    if let Some(column) = column {
                        if *cell_column as usize != column {
                            return None;
                        }
                    }
                    if let Some(row) = row {
                        if *cell_row as usize != row {
                            return None;
                        }
                    }
                    Some(cell.text.chars().count())
                })
                .max()
                .unwrap_or(0);
        }
        let (rows, columns) = self.sheet_bounds();
        let mut longest = 0_usize;
        let row_range = match row {
            Some(row) => row..row.saturating_add(1),
            None => 0..rows.min(256),
        };
        let column_range = match column {
            Some(column) => column..column.saturating_add(1),
            None => 0..columns.min(64),
        };
        for cell_row in row_range {
            for cell_column in column_range.clone() {
                longest = longest.max(self.display_cell(cell_row, cell_column).0.chars().count());
            }
        }
        longest
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

    fn display_cell(&self, row: usize, column: usize) -> (String, bool, Option<u32>) {
        if self.browse.is_some() {
            return self
                .browse_cell(row, column)
                .map(|cell| (cell.text.clone(), cell.numeric, cell.format_color))
                .unwrap_or_default();
        }
        let Ok(cell) = self.cell_ref(row, column) else {
            return (String::new(), false, None);
        };
        let value = self.document.value(cell);
        let numeric = matches!(value, CellValue::Number(_));
        (display_value(&value), numeric, None)
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
                    usize::try_from(sheet.rows)
                        .unwrap_or(usize::MAX)
                        .max(VISIBLE_ROWS as usize)
                        .max(1),
                    usize::try_from(sheet.columns)
                        .unwrap_or(usize::MAX)
                        .max(VISIBLE_COLUMNS as usize)
                        .max(1),
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

fn persist_native(
    path: &Path,
    branch: BranchId,
    actor: Actor,
    timestamp: i64,
    command: Command,
) -> Result<Document, ApplyError> {
    let mut store = omasheets_store::Store::open(path).map_err(store_apply_error)?;
    store
        .append(branch, actor, timestamp, command)
        .map_err(store_apply_error)?;
    Ok(store.document(branch).map_err(store_apply_error)?.clone())
}

fn store_apply_error(error: omasheets_store::StoreError) -> ApplyError {
    match error {
        omasheets_store::StoreError::Apply(error) => error,
        other => ApplyError::InvalidPresentation(other.to_string()),
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

    #[test]
    fn column_and_row_sizes_follow_the_session() {
        let mut session = SpreadsheetSession::open("book").expect("session");
        assert_eq!(session.column_width_px(0), 72.0);
        assert_eq!(session.row_height_px(0), 22.0);
        session.set_column_width_px(0, 180.0);
        assert_eq!(session.column_width_px(0), 180.0);
        session.set_column_width_px(1, 5.0);
        assert_eq!(session.column_width_px(1), 24.0);
        session.set_row_height_px(0, 40.0);
        assert_eq!(session.row_height_px(0), 40.0);
        session.set_row_height_px(1, 1.0);
        assert_eq!(session.row_height_px(1), 12.0);
        session.autofit_column(0);
        assert_eq!(session.column_width_px(0), 24.0);
        session.autofit_row(0);
        assert_eq!(session.row_height_px(0), 22.0);
    }

    #[test]
    fn percent_format_is_visible_on_an_opened_sheet() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "omasheets-format-{}-{nonce}.xlsx",
            std::process::id()
        ));
        write_percent_workbook(&path);
        let session = SpreadsheetSession::open_xlsx(&path).expect("open formatted workbook");
        let _ = std::fs::remove_file(&path);
        let cells = session.visible_cells();
        let percent = cells
            .iter()
            .find(|cell| cell.row == 0 && cell.column == 0)
            .expect("A1");
        assert_eq!(percent.text, "6.5%");
        assert!(percent.numeric);
        let text = cells
            .iter()
            .find(|cell| cell.row == 0 && cell.column == 1)
            .expect("B1");
        assert_eq!(text.text, "Hello");
        assert!(!text.numeric);
        let height = session.row_height_px(0);
        assert!(
            (height - 44.0).abs() < 0.05,
            "row height {height} should follow the 30-point row"
        );
        assert_eq!(session.row_height_px(1), 22.0);
        let scientific = cells
            .iter()
            .find(|cell| cell.row == 0 && cell.column == 2)
            .expect("C1");
        assert_eq!(scientific.text, "1.23E+03");
        assert_eq!(scientific.font, Some(0xFF0000));
    }

    fn write_percent_workbook(path: &std::path::Path) {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let parts = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
            ),
            (
                "xl/styles.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="2"><numFmt numFmtId="164" formatCode="0.0%"/><numFmt numFmtId="165" formatCode="[Red]0.00E+00"/></numFmts><fonts count="1"><font/></fonts><fills count="1"><fill/></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf numFmtId="0"/></cellStyleXfs><cellXfs count="3"><xf numFmtId="0"/><xf numFmtId="164"/><xf numFmtId="165"/></cellXfs></styleSheet>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1" ht="30" customHeight="1"><c r="A1" s="1"><v>0.065</v></c><c r="B1" t="inlineStr"><is><t>Hello</t></is></c><c r="C1" s="2"><v>1234</v></c></row></sheetData></worksheet>"#,
            ),
        ];
        for (name, body) in parts {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn an_opened_workbook_recalculates_after_an_edit() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "omasheets-edit-{}-{nonce}.xlsx",
            std::process::id()
        ));
        write_edit_workbook(&path);
        let mut session = SpreadsheetSession::open_xlsx(&path).expect("open");
        let _ = std::fs::remove_file(&path);
        let text = |session: &SpreadsheetSession, column: usize| {
            session
                .visible_cells()
                .into_iter()
                .find(|cell| cell.row == 0 && cell.column == column)
                .map(|cell| cell.text)
                .unwrap_or_default()
        };
        assert_eq!(text(&session, 0), "2");
        assert_eq!(text(&session, 1), "3");
        session.select(0, 0).unwrap();
        session.set_formula_draft("10".into());
        session.commit_edit().unwrap();
        assert_eq!(text(&session, 0), "10");
        assert_eq!(text(&session, 1), "11");
        session.select(0, 1).unwrap();
        session.set_formula_draft("=A1*4".into());
        session.commit_edit().unwrap();
        assert_eq!(text(&session, 1), "40");
        assert_eq!(session.formula_draft(), "=A1*4");
    }

    #[test]
    fn open_xlsx_bytes_shows_engine_values() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "omasheets-bytes-src-{}-{nonce}.xlsx",
            std::process::id()
        ));
        write_edit_workbook(&path);
        let bytes = std::fs::read(&path).expect("read");
        let _ = std::fs::remove_file(&path);
        let session = SpreadsheetSession::open_xlsx_bytes(bytes, "bytes-book").expect("open bytes");
        let a1 = session
            .visible_cells()
            .into_iter()
            .find(|cell| cell.row == 0 && cell.column == 0)
            .map(|cell| cell.text)
            .unwrap_or_default();
        assert_eq!(a1, "2");
    }

    #[test]
    fn open_omasheets_bytes_loads_native_document() {
        use crate::NATIVE_MEDIA_TYPE;

        let path = unique_temp("native-open", "omasheets");
        write_native_grid(&path);
        let bytes = std::fs::read(&path).expect("read");
        cleanup_native(&path);
        let session =
            SpreadsheetSession::open_bytes(&bytes, "native-book", NATIVE_MEDIA_TYPE).expect("open");
        assert!(!session.is_browsing());
        assert!(session.is_native_durable());
        assert_eq!(session.active_sheet_name(), Some("Sheet"));
        assert_eq!(
            crate::sniff_spreadsheet_media_type(&bytes),
            Some(NATIVE_MEDIA_TYPE)
        );
        let sniffed = SpreadsheetSession::open_bytes(&bytes, "native-book", "").expect("sniff");
        assert!(sniffed.is_native_durable());
        assert_eq!(sniffed.active_sheet_name(), Some("Sheet"));
        let octet =
            SpreadsheetSession::open_bytes(&bytes, "native-book", "application/octet-stream")
                .expect("octet-stream");
        assert!(octet.is_native_durable());
        assert!(matches!(
            SpreadsheetSession::open_bytes(b"not a spreadsheet", "x", ""),
            Err(crate::LoadError::UnsupportedMediaType(_))
        ));
    }

    #[test]
    fn native_durable_bytes_roundtrip_keeps_edits() {
        let path = unique_temp("native-durable", "omasheets");
        write_native_grid(&path);
        let mut session = SpreadsheetSession::open_omasheets(&path).expect("open native");
        assert!(session.is_native_durable());
        let sheet = *session.document().sheets().last().expect("sheet");
        session
            .apply_command(Command::SetValue {
                sheet,
                a1: "A1".into(),
                value: Literal::Number(42.0),
            })
            .expect("set A1");
        let cell = session.document().resolve_a1(sheet, "A1").expect("A1");
        assert_eq!(session.document().value(cell), CellValue::Number(42.0));
        let bytes = session
            .durable_bytes()
            .expect("export")
            .expect("native bytes");
        assert_eq!(
            crate::sniff_spreadsheet_media_type(&bytes),
            Some(crate::NATIVE_MEDIA_TYPE)
        );
        let reopened = SpreadsheetSession::open_omasheets_bytes(&bytes, "reopen").expect("reopen");
        assert!(reopened.is_native_durable());
        let sheet = *reopened.document().sheets().last().expect("sheet");
        let cell = reopened.document().resolve_a1(sheet, "A1").expect("A1");
        assert_eq!(reopened.document().value(cell), CellValue::Number(42.0));
        cleanup_native(&path);

        let xlsx = unique_temp("xlsx-no-export", "xlsx");
        write_edit_workbook(&xlsx);
        let mut browsing = SpreadsheetSession::open_xlsx(&xlsx).expect("xlsx");
        assert!(!browsing.is_native_durable());
        assert!(browsing.durable_bytes().expect("no export").is_none());
        let _ = std::fs::remove_file(&xlsx);
    }

    fn unique_temp(label: &str, extension: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "omasheets-{label}-{}-{nonce}.{extension}",
            std::process::id()
        ))
    }

    fn write_native_grid(path: &Path) {
        use omasheets_core::{Actor, ActorKind, DocumentId, ObjectId};
        use omasheets_store::Store;

        let actor = Actor::new(ActorKind::Human, "host");
        let mut store = Store::create(
            path,
            DocumentId(ObjectId::from_seed("native-book")),
            "native-book",
            actor.clone(),
            1,
        )
        .expect("create store");
        let branch = store.branch_id("main").expect("main");
        store
            .append(
                branch,
                actor.clone(),
                2,
                Command::AddSheet {
                    name: "Sheet".into(),
                },
            )
            .expect("sheet");
        let sheet = *store
            .document(branch)
            .expect("doc")
            .sheets()
            .last()
            .expect("sheet id");
        store
            .append(
                branch,
                actor.clone(),
                3,
                Command::AddColumns {
                    sheet,
                    count: 8,
                    at: 0,
                },
            )
            .expect("cols");
        store
            .append(
                branch,
                actor,
                4,
                Command::AddRows {
                    sheet,
                    count: 8,
                    at: 0,
                    table: None,
                },
            )
            .expect("rows");
        store.close().expect("checkpoint");
    }

    fn cleanup_native(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn write_edit_workbook(path: &std::path::Path) {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let parts = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>2</v></c><c r="B1"><f>A1+1</f><v>3</v></c></row></sheetData></worksheet>"#,
            ),
        ];
        for (name, body) in parts {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
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
