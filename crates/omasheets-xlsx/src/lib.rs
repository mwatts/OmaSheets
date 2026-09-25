//! Bounded `.xlsx` import into the owned OmaSheets M0 calculation engine.
//!
//! Date-formatted cells are imported as the raw serial numbers the file stores,
//! matching `omasheets_calc::serial_date`; workbooks that declare the 1904 date
//! system are rejected rather than silently offset by 1462 days.

use calamine::{Cell, CellErrorType, Data, Range, Reader, Xlsx, XlsxFormulaMetadata};
use omasheets_calc::pivot::{
    PivotAggregate, PivotCache, PivotCacheField, PivotDataField, PivotDateFilter, PivotDateGroup,
    PivotGroupBy, PivotScalar, PivotTable, cache_datetime_serial,
};
use omasheets_calc::serial_date::DATE_SYSTEM;
use omasheets_calc::{
    CalcError, CellId, FormulaError, StructuredColumn, StructuredTable, Value, Workbook,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportLimits {
    pub max_sheets: usize,
    pub max_cells: usize,
    pub max_formulas: usize,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_sheets: 256,
            max_cells: 2_000_000,
            max_formulas: 1_000_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SheetInfo {
    pub index: u32,
    pub name: String,
    /// Last occupied source row plus one, including formula-only cells.
    pub rows: usize,
    /// Last occupied source column plus one, including formula-only cells.
    pub columns: usize,
}

/// One occupied source cell, retained for bounded conversion into the native
/// event model. `stored` is the cached workbook value; `formula` is present
/// even when the owned calculation engine cannot compile it.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportedCell {
    pub cell: CellId,
    pub stored: Value,
    pub formula: Option<String>,
}

/// Upper bound on distinct unsupported function names kept in a report, so a
/// hostile workbook cannot inflate the bounded output.
pub const MAX_REPORTED_FUNCTIONS: usize = 128;
const MAX_FUNCTION_NAME_CHARS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsupportedFormula {
    pub cell: CellId,
    /// The structured compile error, kept so reports can group by kind and by
    /// function name without re-parsing the bounded reason text.
    pub error: FormulaError,
    pub reason: String,
}

impl UnsupportedFormula {
    /// Stable label for the kind of compile failure.
    pub fn kind(&self) -> &'static str {
        formula_error_kind(&self.error)
    }
}

pub fn formula_error_kind(error: &FormulaError) -> &'static str {
    match error {
        FormulaError::Empty => "empty",
        FormulaError::UnexpectedToken(_) => "syntax",
        FormulaError::UnsupportedFunction(_) => "unsupported_function",
        FormulaError::InvalidReference(_) => "invalid_reference",
        FormulaError::UnknownSheet(_) => "unknown_sheet",
        FormulaError::ExternalReference(_) => "external_reference",
        FormulaError::UnknownName(_) => "unknown_name",
        FormulaError::UnknownTable(_)
        | FormulaError::UnknownTableColumn { .. }
        | FormulaError::InvalidStructuredReference(_) => "structured_reference",
        FormulaError::UnsupportedName(_) => "unsupported_name",
        FormulaError::RangeTooLarge => "range_too_large",
        FormulaError::Cycle(_) => "cycle",
    }
}

/// Bounded, serialisable summary of one owned-engine import; the JSON printed
/// by `omasheets-xlsx-score` and embedded per workbook by `omasheets-corpus`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScoreReport {
    pub schema: u8,
    pub engine: String,
    pub date_system: String,
    pub source_sha256: String,
    pub sheets: usize,
    pub formula_cells_observed: usize,
    pub formula_cells_loaded: usize,
    pub formula_cells_compared: usize,
    pub stored_values_matched: usize,
    pub stored_values_mismatched: usize,
    pub unsupported_formulas: usize,
    /// Distinct unsupported function names and how many formula cells named
    /// each, capped at [`MAX_REPORTED_FUNCTIONS`] entries.
    pub unsupported_functions: BTreeMap<String, usize>,
    /// Compile-failure kinds and how many formula cells hit each.
    pub unsupported_reasons: BTreeMap<String, usize>,
    /// Syntax failures grouped by a fixed token class, never formula text.
    #[serde(default)]
    pub syntax_failure_tokens: BTreeMap<String, usize>,
    /// Fixed failure classes, with no source references or cell values.
    #[serde(default)]
    pub reference_failure_kinds: BTreeMap<String, usize>,
    #[serde(default)]
    pub mismatch_value_kinds: BTreeMap<String, usize>,
    /// Sheet entries without a worksheet part that the importer skipped.
    #[serde(default)]
    pub skipped_sheets: Vec<String>,
}

pub const ENGINE_NAME: &str = "omasheets-owned-m0";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParitySummary {
    pub formula_cells_observed: usize,
    pub formula_cells_loaded: usize,
    pub formula_cells_compared: usize,
    pub stored_values_matched: usize,
    pub stored_values_mismatched: usize,
    pub unsupported_formulas: usize,
}

pub struct ImportedWorkbook {
    pub workbook: Workbook,
    pub sheets: Vec<SheetInfo>,
    pub source_sha256: String,
    /// Always `"1900"`: the importer refuses every other date system.
    pub date_system: &'static str,
    pub unsupported: Vec<UnsupportedFormula>,
    /// Sheets named in `xl/workbook.xml` without a worksheet part, skipped by
    /// [`import_xlsx`]'s in-memory repair; empty for well-formed packages.
    pub skipped_sheets: Vec<String>,
    source_cells: Vec<ImportedCell>,
    compiled_cells: Vec<usize>,
    formula_cells_observed: usize,
    formula_cells_loaded: usize,
}

impl ImportedWorkbook {
    /// Occupied source cells in sheet/row/column order. This projection is
    /// bounded by the same import limits as the calculation workbook.
    pub fn source_cells(&self) -> &[ImportedCell] {
        &self.source_cells
    }

    /// The compared formula cells whose recalculated value differs from the
    /// stored one, with both values, for tooling that investigates
    /// mismatches; the score report itself stays aggregate.
    pub fn mismatched_cells(&self) -> impl Iterator<Item = (CellId, &Value, Value)> {
        self.compiled_cells
            .iter()
            .map(|index| &self.source_cells[*index])
            .map(|source| {
                (
                    source.cell,
                    &source.stored,
                    self.workbook.value(source.cell),
                )
            })
            .filter(|(_, stored, calculated)| !values_match(stored, calculated))
    }

    pub fn parity(&self) -> ParitySummary {
        let stored_values_matched = self
            .compiled_cells
            .iter()
            .map(|index| &self.source_cells[*index])
            .filter(|source| values_match(&source.stored, &self.workbook.value(source.cell)))
            .count();
        let formula_cells_compared = self.compiled_cells.len();
        ParitySummary {
            formula_cells_observed: self.formula_cells_observed,
            formula_cells_loaded: self.formula_cells_loaded,
            formula_cells_compared,
            stored_values_matched,
            stored_values_mismatched: formula_cells_compared - stored_values_matched,
            unsupported_formulas: self.unsupported.len(),
        }
    }

    /// Distinct unsupported function names with formula-cell counts. Names are
    /// truncated and the map is capped so the report stays bounded.
    pub fn unsupported_functions(&self) -> BTreeMap<String, usize> {
        let mut functions = BTreeMap::new();
        for unsupported in &self.unsupported {
            let FormulaError::UnsupportedFunction(name) = &unsupported.error else {
                continue;
            };
            let name: String = name.chars().take(MAX_FUNCTION_NAME_CHARS).collect();
            if functions.len() >= MAX_REPORTED_FUNCTIONS && !functions.contains_key(&name) {
                continue;
            }
            *functions.entry(name).or_insert(0) += 1;
        }
        functions
    }

    /// Compile-failure kinds with formula-cell counts.
    pub fn unsupported_reasons(&self) -> BTreeMap<String, usize> {
        let mut reasons = BTreeMap::new();
        for unsupported in &self.unsupported {
            *reasons.entry(unsupported.kind().to_string()).or_insert(0) += 1;
        }
        reasons
    }

    pub fn report(&self) -> ScoreReport {
        let parity = self.parity();
        let mut reference_failure_kinds = BTreeMap::new();
        for failure in &self.unsupported {
            if let FormulaError::InvalidReference(reference) = &failure.error {
                let kind = match reference.as_str() {
                    "range endpoint is number" => "numeric_range_endpoint",
                    "range endpoint is function" => "dynamic_range_endpoint",
                    "range endpoints must be references" => "non_reference_endpoint",
                    "range endpoints cross sheets" => "cross_sheet_range",
                    "" => "empty_reference",
                    token if token.bytes().all(|b| b.is_ascii_alphabetic() || b == b'$') => {
                        "column_without_row"
                    }
                    token if token.bytes().all(|b| b.is_ascii_digit() || b == b'$') => {
                        "row_without_column"
                    }
                    _ => "invalid_a1",
                };
                *reference_failure_kinds.entry(kind.to_string()).or_insert(0) += 1;
            }
        }
        let mut mismatch_value_kinds = BTreeMap::new();
        for (_, stored, calculated) in self.mismatched_cells() {
            let kind = format!("{} -> {}", value_kind(stored), value_kind(&calculated));
            *mismatch_value_kinds.entry(kind).or_insert(0) += 1;
        }
        let mut syntax_failure_tokens = BTreeMap::new();
        for failure in &self.unsupported {
            let FormulaError::UnexpectedToken(offset) = failure.error else {
                continue;
            };
            let Ok(index) = self
                .source_cells
                .binary_search_by_key(&failure.cell, |cell| cell.cell)
            else {
                continue;
            };
            let Some(source) = self.source_cells[index].formula.as_deref() else {
                continue;
            };
            let source = source.strip_prefix('=').unwrap_or(source);
            let token = match source.as_bytes().get(offset) {
                Some(b'{') | Some(b'}') => "array_brace",
                Some(b':') => "range_colon",
                Some(b',') => "comma",
                Some(b';') => "semicolon",
                Some(b'!') => "sheet_separator",
                Some(b'[') | Some(b']') => "square_bracket",
                Some(b'@') => "implicit_intersection",
                Some(b'\\') => "backslash_identifier",
                Some(b'(') | Some(b')') => "parenthesis",
                Some(byte) if !byte.is_ascii() => "non_ascii_identifier",
                Some(byte) if byte.is_ascii_alphabetic() => "identifier",
                None => "end_of_formula",
                _ => "other",
            };
            *syntax_failure_tokens.entry(token.to_string()).or_insert(0) += 1;
        }
        ScoreReport {
            schema: 2,
            engine: ENGINE_NAME.into(),
            date_system: self.date_system.into(),
            source_sha256: self.source_sha256.clone(),
            sheets: self.sheets.len(),
            formula_cells_observed: parity.formula_cells_observed,
            formula_cells_loaded: parity.formula_cells_loaded,
            formula_cells_compared: parity.formula_cells_compared,
            stored_values_matched: parity.stored_values_matched,
            stored_values_mismatched: parity.stored_values_mismatched,
            unsupported_formulas: parity.unsupported_formulas,
            unsupported_functions: self.unsupported_functions(),
            unsupported_reasons: self.unsupported_reasons(),
            syntax_failure_tokens,
            reference_failure_kinds,
            mismatch_value_kinds,
            skipped_sheets: self.skipped_sheets.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportError {
    Open(String),
    Read(String),
    TooManySheets { observed: usize, maximum: usize },
    TooManyCells { observed: usize, maximum: usize },
    TooManyFormulas { observed: usize, maximum: usize },
    UnsupportedDateSystem { observed: &'static str },
}

impl fmt::Display for ImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(error) => write!(formatter, "could not open xlsx: {error}"),
            Self::Read(error) => write!(formatter, "could not read xlsx: {error}"),
            Self::TooManySheets { observed, maximum } => {
                write!(
                    formatter,
                    "workbook has {observed} sheets; limit is {maximum}"
                )
            }
            Self::TooManyCells { observed, maximum } => {
                write!(
                    formatter,
                    "workbook spans {observed} cells; limit is {maximum}"
                )
            }
            Self::TooManyFormulas { observed, maximum } => {
                write!(
                    formatter,
                    "workbook has {observed} formulas; limit is {maximum}"
                )
            }
            Self::UnsupportedDateSystem { observed } => {
                write!(
                    formatter,
                    "workbook uses the {observed} date system; only the {DATE_SYSTEM} date system is supported"
                )
            }
        }
    }
}

impl std::error::Error for ImportError {}

/// How many workbooks may be open along one external-link chain. One hop is
/// required; the cap stops a cycle or a long chain from recursing without
/// bound. A workbook already on the chain is not opened again.
const MAX_EXTERNAL_WORKBOOKS: usize = 8;

pub fn import_xlsx(path: &Path, limits: ImportLimits) -> Result<ImportedWorkbook, ImportError> {
    let mut opening = HashSet::new();
    import_xlsx_inner(path, limits, &mut opening)
}

fn import_xlsx_inner(
    path: &Path,
    limits: ImportLimits,
    opening: &mut HashSet<PathBuf>,
) -> Result<ImportedWorkbook, ImportError> {
    if opening.len() >= MAX_EXTERNAL_WORKBOOKS {
        return Err(ImportError::Open(
            "external workbook chain is too deep".into(),
        ));
    }
    let key = canonical_key(path);
    if !opening.insert(key.clone()) {
        return Err(ImportError::Open("external workbook cycle".into()));
    }
    let imported = import_xlsx_body(path, limits, opening);
    opening.remove(&key);
    imported
}

fn import_xlsx_body(
    path: &Path,
    limits: ImportLimits,
    opening: &mut HashSet<PathBuf>,
) -> Result<ImportedWorkbook, ImportError> {
    let source_sha256 = hash_file(path)?;
    let (mut source, skipped_sheets) = open_repaired(path)?;
    check_date_system(source.has_1904_epoch())?;
    let sheet_names = source.sheet_names();
    if sheet_names.len() > limits.max_sheets {
        return Err(ImportError::TooManySheets {
            observed: sheet_names.len(),
            maximum: limits.max_sheets,
        });
    }

    // Read the names from the package part rather than through Calamine,
    // which drops each name's `localSheetId` scope.
    let defined_names = read_defined_names(path)?;
    let tables = read_tables(path).unwrap_or_default();
    // External targets are loaded before this workbook's formulas compile, so
    // a reference sees the calculated cell. The stored link cache is used
    // when that file is absent, already on the chain, or only a base-name
    // collision of an absolute target whose cache is already populated.
    let external = external_cells_for_import(path, limits, opening);
    let array_formulas = read_array_formulas(path).unwrap_or_default();
    let pivots = read_pivots(path, limits);
    let mut ranges = Vec::with_capacity(sheet_names.len());
    let mut observed_cells = 0_usize;
    let mut observed_formulas = 0_usize;
    for name in &sheet_names {
        let values = source
            .worksheet_range(name)
            .map_err(|error| ImportError::Read(error.to_string()))?;
        observed_cells =
            observed_cells.saturating_add(values.width().saturating_mul(values.height()));
        if observed_cells > limits.max_cells {
            return Err(ImportError::TooManyCells {
                observed: observed_cells,
                maximum: limits.max_cells,
            });
        }
        let formulas = read_formulas(&mut source, name)?;
        observed_formulas = observed_formulas.saturating_add(
            formulas
                .used_cells()
                .filter(|(_, _, formula)| !formula.is_empty())
                .count(),
        );
        if observed_formulas > limits.max_formulas {
            return Err(ImportError::TooManyFormulas {
                observed: observed_formulas,
                maximum: limits.max_formulas,
            });
        }
        ranges.push((name.clone(), values, formulas));
    }
    let mut imported = import_ranges_with_names(
        ranges,
        defined_names,
        &tables,
        external.cells,
        external.sheets,
        &array_formulas,
        pivots,
        source_sha256,
        limits,
    )?;
    imported.skipped_sheets = skipped_sheets;
    Ok(imported)
}

/// Reads a sheet's formulas, expanding shared formulas from their anchor
/// cell. Calamine's `worksheet_formula` shifts a derived cell from the
/// top-left of the shared `ref` range instead; Excel anchors a group at its
/// first cell, which need not be that corner (a corner cell can carry its own
/// formula), and the corpus has sheets whose derived cells came out shifted
/// by a column as a result. A derived cell whose anchor appears later in the
/// stream is resolved at the end; one whose anchor never appears is skipped.
fn read_formulas<RS: Read + Seek>(
    source: &mut Xlsx<RS>,
    name: &str,
) -> Result<Range<String>, ImportError> {
    let read_error = |error: calamine::XlsxError| ImportError::Read(error.to_string());
    // Chart and dialog sheets have no cells; Calamine's own range readers
    // return an empty range for them and so does this one.
    let mut reader = match source.worksheet_cells_reader(name) {
        Ok(reader) => reader,
        Err(calamine::XlsxError::NotAWorksheet(_)) => return Ok(Range::default()),
        Err(error) => return Err(read_error(error)),
    };
    let mut anchors: HashMap<usize, ((u32, u32), String)> = HashMap::new();
    let mut cells = Vec::new();
    let mut pending = Vec::new();
    while let Some(record) = reader
        .next_cell_with_formula_metadata()
        .map_err(read_error)?
    {
        match record.formula {
            Some(XlsxFormulaMetadata::Normal { formula }) => {
                cells.push(Cell::new(record.pos, formula));
            }
            Some(XlsxFormulaMetadata::Shared {
                shared_index,
                formula,
                ..
            }) => {
                anchors.insert(shared_index, (record.pos, formula.clone()));
                cells.push(Cell::new(record.pos, formula));
            }
            Some(XlsxFormulaMetadata::SharedDerived { shared_index }) => {
                pending.push((record.pos, shared_index));
            }
            _ => {}
        }
    }
    for (position, shared_index) in pending {
        if let Some((anchor, template)) = anchors.get(&shared_index) {
            let formula =
                calamine::expand_shared_formula(template, *anchor, position).map_err(read_error)?;
            cells.push(Cell::new(position, formula));
        }
    }
    Ok(Range::from_sparse(cells))
}

/// Opens a workbook, and when Calamine refuses it because a `<sheet>` entry
/// carries an empty or dangling relationship id, opens an in-memory copy of
/// the package whose `xl/workbook.xml` omits those entries.
///
/// Every such sheet in the frozen Enron sample is a `veryHidden` legacy macro
/// module left behind by an `.xls` conversion: it has no worksheet part, so
/// nothing is lost by skipping it, and the names of the skipped sheets are
/// reported so the omission is never silent. The source file is never
/// modified; the repair exists only in memory for this import.
/// A workbook reader over either the file itself or the in-memory repaired
/// copy, with the names of the sheet entries the repair skipped.
type OpenedWorkbook = (Xlsx<Box<dyn ReadSeek>>, Vec<String>);

fn open_repaired(path: &Path) -> Result<OpenedWorkbook, ImportError> {
    let file = File::open(path).map_err(|error| ImportError::Open(error.to_string()))?;
    match Xlsx::new(Box::new(BufReader::new(file)) as Box<dyn ReadSeek>) {
        Ok(workbook) => Ok((workbook, Vec::new())),
        Err(calamine::XlsxError::RelationshipNotFound) => {
            let (repaired, skipped) = repair_dangling_sheets(path)?;
            if skipped.is_empty() {
                return Err(ImportError::Open("Relationship not found".into()));
            }
            let workbook = Xlsx::new(Box::new(Cursor::new(repaired)) as Box<dyn ReadSeek>)
                .map_err(|error| ImportError::Open(error.to_string()))?;
            Ok((workbook, skipped))
        }
        Err(error) => Err(ImportError::Open(error.to_string())),
    }
}

pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// Largest `xl/workbook.xml` the repair will rewrite; real workbook parts are
/// a few kilobytes, and the copy is held in memory.
const MAX_WORKBOOK_PART_BYTES: u64 = 4 * 1024 * 1024;

/// Rebuilds the package without the `<sheet>` entries whose relationship id
/// is empty or absent from `xl/_rels/workbook.xml.rels`, copying every other
/// part byte for byte. Returns the new package and the skipped sheet names.
fn repair_dangling_sheets(path: &Path) -> Result<(Vec<u8>, Vec<String>), ImportError> {
    let file = File::open(path).map_err(|error| ImportError::Open(error.to_string()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| ImportError::Open(error.to_string()))?;
    let relationships = read_part(&mut archive, "xl/_rels/workbook.xml.rels")?;
    let workbook = read_part(&mut archive, "xl/workbook.xml")?;
    let known_ids: std::collections::HashSet<String> =
        attribute_values(&relationships, "Id").into_iter().collect();
    let (rewritten, skipped) = drop_dangling_sheets(&workbook, &known_ids);
    if skipped.is_empty() {
        return Ok((Vec::new(), skipped));
    }

    let mut output = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|error| ImportError::Open(error.to_string()))?;
        if entry.name() == "xl/workbook.xml" {
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            output
                .start_file("xl/workbook.xml", options)
                .and_then(|()| output.write_all(rewritten.as_bytes()).map_err(Into::into))
                .map_err(|error| ImportError::Open(error.to_string()))?;
        } else {
            output
                .raw_copy_file(entry)
                .map_err(|error| ImportError::Open(error.to_string()))?;
        }
    }
    let cursor = output
        .finish()
        .map_err(|error| ImportError::Open(error.to_string()))?;
    Ok((cursor.into_inner(), skipped))
}

/// Values of every `name="…"` attribute in `xml`, in document order. The
/// package parts involved are machine-written, so a lexical scan is enough.
/// Reads one small XML part of the package as text, refusing parts over
/// [`MAX_WORKBOOK_PART_BYTES`].
fn read_part(archive: &mut zip::ZipArchive<File>, name: &str) -> Result<String, ImportError> {
    let mut part = archive
        .by_name(name)
        .map_err(|error| ImportError::Open(format!("{name}: {error}")))?;
    if part.size() > MAX_WORKBOOK_PART_BYTES {
        return Err(ImportError::Open(format!(
            "{name} exceeds the workbook part size limit"
        )));
    }
    let mut text = String::new();
    part.read_to_string(&mut text)
        .map_err(|error| ImportError::Open(format!("{name}: {error}")))?;
    Ok(text)
}

/// A defined name from `xl/workbook.xml`. `sheet` is the name of the scope
/// sheet for a `localSheetId` name and `None` for a workbook-level name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinedName {
    pub sheet: Option<String>,
    pub name: String,
    pub definition: String,
}

/// Reads every `<definedName>` of the workbook part with its scope. A
/// `localSheetId` is an index into the part's own `<sheet>` list, so it is
/// resolved to a sheet name here, before any repair renumbers the sheets; a
/// name whose scope index points past that list is dropped.
fn read_defined_names(path: &Path) -> Result<Vec<DefinedName>, ImportError> {
    let file = File::open(path).map_err(|error| ImportError::Open(error.to_string()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| ImportError::Open(error.to_string()))?;
    let workbook = read_part(&mut archive, "xl/workbook.xml")?;
    Ok(parse_defined_names(&workbook))
}

fn parse_defined_names(workbook_xml: &str) -> Vec<DefinedName> {
    let mut sheets = Vec::new();
    let mut rest = workbook_xml;
    while let Some(start) = rest.find("<sheet ") {
        let Some(length) = rest[start..].find('>') else {
            break;
        };
        let element = &rest[start..start + length];
        if let Some(name) = attribute(element, "name") {
            sheets.push(name);
        }
        rest = &rest[start + length + 1..];
    }
    let mut names = Vec::new();
    let mut rest = workbook_xml;
    while let Some(start) = rest.find("<definedName ") {
        let after_tag = &rest[start..];
        let Some(tag_end) = after_tag.find('>') else {
            break;
        };
        let tag = &after_tag[..tag_end];
        let self_closing = tag.ends_with('/');
        let body_start = tag_end + 1;
        let (definition, consumed) = if self_closing {
            (String::new(), body_start)
        } else {
            match after_tag[body_start..].find("</definedName>") {
                Some(end) => (
                    unescape_xml(&after_tag[body_start..body_start + end]),
                    body_start + end,
                ),
                None => break,
            }
        };
        rest = &after_tag[consumed..];
        let Some(name) = attribute(tag, "name") else {
            continue;
        };
        let sheet = match attribute(tag, "localSheetId") {
            None => None,
            Some(index) => match index.parse::<usize>().ok().and_then(|i| sheets.get(i)) {
                Some(sheet) => Some(sheet.clone()),
                None => continue,
            },
        };
        names.push(DefinedName {
            sheet,
            name,
            definition,
        });
    }
    names
}

/// The value of attribute `name` in one start tag, unescaped.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!(" {name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')?;
    Some(unescape_xml(&tag[start..start + end]))
}

/// Decodes the five XML entities and numeric character references; anything
/// else is kept as written.
fn unescape_xml(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find(';').filter(|end| *end <= 10) else {
            output.push('&');
            rest = after;
            continue;
        };
        let entity = &after[..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix('#')
                .and_then(|number| match number.strip_prefix('x') {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse::<u32>().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(character) => {
                output.push(character);
                rest = &after[end + 1..];
            }
            None => {
                output.push('&');
                rest = after;
            }
        }
    }
    output.push_str(rest);
    output
}

fn attribute_values(xml: &str, name: &str) -> Vec<String> {
    let needle = format!("{name}=\"");
    let mut values = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&needle) {
        let after = &rest[start + needle.len()..];
        let Some(end) = after.find('"') else { break };
        values.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    values
}

/// One cell registered on the workbook's external cache before formulas compile.
struct CachedExternalCell {
    link_index: u32,
    book_file: Option<String>,
    sheet: String,
    row: u32,
    column: u32,
    value: Value,
}

struct ExternalLinkRecord {
    index: u32,
    book_file: Option<String>,
    /// Local file this link may open. Never a network URL or an absolute path
    /// outside the source workbook's directory.
    path: Option<PathBuf>,
    /// `path` is only the file name of an absolute or `file://` target. A
    /// populated cache must not be replaced by whatever happens to share that
    /// name in a flat directory.
    basename_only: bool,
    sheets: Vec<String>,
    /// Sheets whose refresh failed. Their cached cells are kept. A cell the
    /// part does not list is `#REF!`.
    broken: Vec<String>,
    cached: Vec<CachedExternalCell>,
}

struct ExternalSheetNote {
    link_index: u32,
    book_file: Option<String>,
    sheet: String,
    broken: bool,
}

struct ExternalCacheLoad {
    cells: Vec<CachedExternalCell>,
    sheets: Vec<ExternalSheetNote>,
}

fn canonical_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Cells and known sheets for every external link of `path`. A relative
/// target that exists next to the source is calculated and wins over the link
/// part. A missing file, a network target, a workbook already being imported,
/// or a populated cache whose only local hit is the base name of an absolute
/// target keeps the part's cache.
fn external_cells_for_import(
    path: &Path,
    limits: ImportLimits,
    opening: &mut HashSet<PathBuf>,
) -> ExternalCacheLoad {
    let Ok(links) = read_external_links(path) else {
        return ExternalCacheLoad {
            cells: Vec::new(),
            sheets: Vec::new(),
        };
    };
    let mut cells = Vec::new();
    let mut sheets = Vec::new();
    for link in links {
        let keep_cache = link.basename_only && !link.cached.is_empty();
        let calculated = if keep_cache {
            None
        } else {
            link.path.as_deref().and_then(|target| {
                let key = canonical_key(target);
                if opening.contains(&key) {
                    return None;
                }
                import_xlsx_inner(target, limits, opening).ok()
            })
        };
        match calculated {
            Some(imported) => {
                let book_file = link.book_file.clone();
                for sheet in &imported.sheets {
                    sheets.push(ExternalSheetNote {
                        link_index: link.index,
                        book_file: book_file.clone(),
                        sheet: sheet.name.clone(),
                        broken: false,
                    });
                }
                cells.extend(cells_from_imported(&imported, link.index, book_file));
            }
            None => {
                for sheet in &link.sheets {
                    sheets.push(ExternalSheetNote {
                        link_index: link.index,
                        book_file: link.book_file.clone(),
                        sheet: sheet.clone(),
                        broken: false,
                    });
                }
                for sheet in &link.broken {
                    sheets.push(ExternalSheetNote {
                        link_index: link.index,
                        book_file: link.book_file.clone(),
                        sheet: sheet.clone(),
                        broken: true,
                    });
                }
                cells.extend(link.cached);
            }
        }
    }
    ExternalCacheLoad { cells, sheets }
}

fn cells_from_imported(
    imported: &ImportedWorkbook,
    link_index: u32,
    book_file: Option<String>,
) -> Vec<CachedExternalCell> {
    imported
        .source_cells()
        .iter()
        .filter_map(|source| {
            let sheet = imported.sheets.get(source.cell.sheet as usize)?;
            Some(CachedExternalCell {
                link_index,
                book_file: book_file.clone(),
                sheet: sheet.name.clone(),
                row: source.cell.row,
                column: source.cell.column,
                value: imported.workbook.value(source.cell),
            })
        })
        .collect()
}

fn read_external_links(path: &Path) -> Result<Vec<ExternalLinkRecord>, ImportError> {
    let file = File::open(path).map_err(|error| ImportError::Open(error.to_string()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| ImportError::Open(error.to_string()))?;
    let workbook = read_part(&mut archive, "xl/workbook.xml")?;
    let relationships = parse_relationships(
        &read_optional_part(&mut archive, "xl/_rels/workbook.xml.rels").unwrap_or_default(),
    );
    let source_dir = path.parent().unwrap_or(Path::new(""));
    let mut links = Vec::new();
    for (offset, rel_id) in external_reference_ids(&workbook).into_iter().enumerate() {
        let index = (offset + 1) as u32;
        let part_target = relationships
            .iter()
            .find(|relationship| relationship.id == rel_id)
            .map(|relationship| relationship.target.as_str());
        let Some(part_target) = part_target else {
            links.push(ExternalLinkRecord {
                index,
                book_file: None,
                path: None,
                basename_only: false,
                sheets: Vec::new(),
                broken: Vec::new(),
                cached: Vec::new(),
            });
            continue;
        };
        let part = resolve_package_part("xl/workbook.xml", part_target);
        let xml = read_optional_part(&mut archive, &part).unwrap_or_default();
        let link_relationships = parse_relationships(
            &read_optional_part(&mut archive, &package_rels_path(&part)).unwrap_or_default(),
        );
        let book_rel = external_book_relationship_id(&xml);
        let raw_target = book_rel
            .as_ref()
            .and_then(|id| {
                link_relationships
                    .iter()
                    .find(|relationship| relationship.id == *id)
            })
            .or_else(|| {
                link_relationships
                    .iter()
                    .find(|relationship| relationship.kind.ends_with("/externalLinkPath"))
            })
            .map(|relationship| relationship.target.clone());
        let book_file = raw_target.as_deref().and_then(external_book_label);
        let resolved = raw_target
            .as_deref()
            .and_then(|target| resolve_external_target(source_dir, target));
        let (path, basename_only) = match resolved {
            Some((path, basename_only)) => (Some(path), basename_only),
            None => (None, false),
        };
        let (sheets, broken, cached) = parse_external_cache(&xml, index, book_file.clone());
        links.push(ExternalLinkRecord {
            index,
            book_file,
            path,
            basename_only,
            sheets,
            broken,
            cached,
        });
    }
    Ok(links)
}

fn read_optional_part(archive: &mut zip::ZipArchive<File>, name: &str) -> Option<String> {
    read_part(archive, name).ok()
}

struct PackageRelationship {
    id: String,
    kind: String,
    target: String,
}

fn parse_relationships(xml: &str) -> Vec<PackageRelationship> {
    let mut relationships = Vec::new();
    scan_elements(xml, "Relationship", |tag, _| {
        let Some(id) = attribute(tag, "Id") else {
            return;
        };
        let Some(target) = attribute(tag, "Target") else {
            return;
        };
        relationships.push(PackageRelationship {
            id,
            kind: attribute(tag, "Type").unwrap_or_default(),
            target,
        });
    });
    relationships
}

fn external_reference_ids(workbook_xml: &str) -> Vec<String> {
    let mut ids = Vec::new();
    scan_elements(workbook_xml, "externalReference", |tag, _| {
        if let Some(id) = attribute(tag, "r:id") {
            ids.push(id);
        }
    });
    ids
}

fn external_book_relationship_id(external_link_xml: &str) -> Option<String> {
    let mut found = None;
    scan_elements(external_link_xml, "externalBook", |tag, _| {
        if found.is_none() {
            found = attribute(tag, "r:id");
        }
    });
    found
}

fn resolve_package_part(source_part: &str, target: &str) -> String {
    let target = target.replace('\\', "/");
    if let Some(absolute) = target.strip_prefix('/') {
        return absolute.trim_start_matches('/').to_string();
    }
    let mut parts: Vec<&str> = source_part
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("")
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn package_rels_path(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, name)) => format!("{dir}/_rels/{name}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// Path of a link target that may be opened: a relative path under the source
/// directory, or, for an absolute path or `file://` URL, the file name next
/// to the source. Network targets and `..` are not opened. The boolean is
/// true when the path is only that base name.
#[cfg(test)]
fn resolve_external_path(source_dir: &Path, raw_target: &str) -> Option<PathBuf> {
    resolve_external_target(source_dir, raw_target).map(|(path, _)| path)
}

fn resolve_external_target(source_dir: &Path, raw_target: &str) -> Option<(PathBuf, bool)> {
    let (relative, basename_only) = local_relative_target(raw_target)?;
    let candidate = source_dir.join(relative);
    candidate.is_file().then_some((candidate, basename_only))
}

fn local_relative_target(raw_target: &str) -> Option<(PathBuf, bool)> {
    let decoded = percent_decode(raw_target.trim());
    if decoded.is_empty() {
        return None;
    }
    let lower = decoded.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("ftp://")
        || lower.starts_with("mailto:")
        || (lower.contains("://") && !lower.starts_with("file:"))
    {
        return None;
    }
    let path_text = if lower.starts_with("file:") {
        file_url_path(&decoded)?
    } else {
        decoded.replace('\\', "/")
    };
    let path = Path::new(path_text.trim());
    if lower.starts_with("file:") || path.is_absolute() {
        return path
            .file_name()
            .filter(|name| !name.is_empty())
            .map(|name| (PathBuf::from(name), true));
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    let relative: PathBuf = path
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .collect();
    if relative.as_os_str().is_empty() {
        None
    } else {
        Some((relative, false))
    }
}

fn file_url_path(url: &str) -> Option<String> {
    let rest = strip_ascii_prefix(url, "file:")?;
    let rest = rest.strip_prefix("//")?;
    let rest = strip_ascii_prefix(rest, "localhost").unwrap_or(rest);
    let rest = rest.replace('\\', "/");
    if rest.is_empty() || rest == "/" {
        None
    } else {
        Some(rest)
    }
}

fn strip_ascii_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    (text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix))
        .then_some(&text[prefix.len()..])
}

fn external_book_label(raw_target: &str) -> Option<String> {
    let decoded = percent_decode(raw_target.trim());
    let trimmed = decoded.split(['?', '#']).next().unwrap_or(decoded.as_str());
    let segment = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed).trim();
    if segment.is_empty() || segment == "." || segment == ".." || segment.contains(':') {
        None
    } else {
        Some(segment.to_string())
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[index + 1..index + 3]) {
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    decoded.push(value);
                    index += 3;
                    continue;
                }
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn parse_external_cache(
    xml: &str,
    link_index: u32,
    book_file: Option<String>,
) -> (Vec<String>, Vec<String>, Vec<CachedExternalCell>) {
    let mut sheet_names = Vec::new();
    let mut broken_sheets = std::collections::HashSet::new();
    scan_elements(xml, "sheetNames", |_tag, body| {
        scan_elements(body, "sheetName", |tag, _| {
            if let Some(name) = attribute(tag, "val") {
                sheet_names.push(name);
            }
        });
    });
    let mut cells = Vec::new();
    scan_elements(xml, "sheetDataSet", |_tag, body| {
        scan_elements(body, "sheetData", |tag, data| {
            let Some(sheet_id) =
                attribute(tag, "sheetId").and_then(|value| value.parse::<usize>().ok())
            else {
                return;
            };
            let Some(sheet) = sheet_names.get(sheet_id).cloned() else {
                return;
            };
            // A failed refresh keeps the cells the part still lists. A cell
            // it does not list is `#REF!`.
            if attribute(tag, "refreshError").is_some_and(|value| value != "0") {
                broken_sheets.insert(sheet_id);
            }
            scan_elements(data, "row", |_row_tag, row_body| {
                scan_elements(row_body, "cell", |cell_tag, cell_body| {
                    let Some(reference) = attribute(cell_tag, "r") else {
                        return;
                    };
                    let Some((row, column)) = parse_cell_reference(&reference) else {
                        return;
                    };
                    let Some(value) = cached_cell_value(cell_tag, cell_body) else {
                        return;
                    };
                    cells.push(CachedExternalCell {
                        link_index,
                        book_file: book_file.clone(),
                        sheet: sheet.clone(),
                        row,
                        column,
                        value,
                    });
                });
            });
        });
    });
    let mut known_sheets = Vec::new();
    let mut broken = Vec::new();
    for (index, name) in sheet_names.into_iter().enumerate() {
        if broken_sheets.contains(&index) {
            broken.push(name);
        } else {
            known_sheets.push(name);
        }
    }
    (known_sheets, broken, cells)
}

fn cached_cell_value(tag: &str, body: &str) -> Option<Value> {
    let kind = attribute(tag, "t").unwrap_or_default();
    if kind == "s" {
        return None;
    }
    let raw = xml_text_element(body, "v").or_else(|| xml_text_element(body, "t"))?;
    // Text keeps its spaces. `MATCH` against a header such as `   NG   `
    // fails if the cache trims them and the local cell does not.
    if kind == "str" || kind == "inlineStr" {
        // An empty `<v/>` is an empty string. Dropping it makes the cell
        // look missing, and a known sheet then shows that reference as 0.
        return Some(Value::Text(raw));
    }
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match kind.as_str() {
        "b" => Some(Value::Boolean(
            raw == "1" || raw.eq_ignore_ascii_case("true"),
        )),
        "e" => Some(Value::Error(excel_error(raw))),
        _ => raw.parse::<f64>().ok().map(Value::Number),
    }
}

fn excel_error(raw: &str) -> CalcError {
    match raw {
        "#DIV/0!" => CalcError::DivisionByZero,
        "#N/A" => CalcError::NotAvailable,
        "#NAME?" => CalcError::InvalidName,
        "#NULL!" => CalcError::NullIntersection,
        "#NUM!" => CalcError::InvalidNumber,
        "#VALUE!" => CalcError::InvalidValue,
        "#SPILL!" => CalcError::Spill,
        _ => CalcError::InvalidReference,
    }
}

fn xml_text_element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let after = &xml[start + open.len()..];
    if !after.starts_with([' ', '>', '/', '\n', '\r', '\t']) {
        return None;
    }
    let tag_end = after.find('>')?;
    if after[..tag_end].trim_end().ends_with('/') {
        return Some(String::new());
    }
    let content = &after[tag_end + 1..];
    let close = format!("</{tag}>");
    let end = content.find(&close)?;
    Some(unescape_xml(&content[..end]))
}

fn parse_cell_reference(reference: &str) -> Option<(u32, u32)> {
    let normalized = reference.replace('$', "").to_ascii_uppercase();
    let split = normalized.find(|character: char| character.is_ascii_digit())?;
    let (column_text, row_text) = normalized.split_at(split);
    if column_text.is_empty()
        || row_text.is_empty()
        || !column_text.bytes().all(|byte| byte.is_ascii_uppercase())
        || !row_text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let mut column = 0_u32;
    for byte in column_text.bytes() {
        column = column
            .checked_mul(26)?
            .checked_add(u32::from(byte - b'A') + 1)?;
    }
    let row = row_text.parse::<u32>().ok().filter(|value| *value > 0)?;
    if column > 16_384 || row > 1_048_576 {
        return None;
    }
    Some((row - 1, column - 1))
}

fn scan_elements(xml: &str, tag: &str, mut visit: impl FnMut(&str, &str)) {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        if !after.starts_with([' ', '>', '/', '\n', '\r', '\t']) {
            rest = after;
            continue;
        }
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let start_tag = &after[..tag_end];
        if start_tag.trim_end().ends_with('/') {
            visit(start_tag, "");
            rest = &after[tag_end + 1..];
            continue;
        }
        let content = &after[tag_end + 1..];
        let Some(end) = content.find(&close) else {
            break;
        };
        visit(start_tag, &content[..end]);
        rest = &content[end + close.len()..];
    }
}

/// Removes `<sheet …/>` elements whose `r:id` is empty or unknown and returns
/// the rewritten XML with the names of the removed sheets.
fn drop_dangling_sheets(
    workbook_xml: &str,
    known_ids: &std::collections::HashSet<String>,
) -> (String, Vec<String>) {
    let mut output = String::with_capacity(workbook_xml.len());
    let mut skipped = Vec::new();
    let mut rest = workbook_xml;
    while let Some(start) = rest.find("<sheet ") {
        let Some(length) = rest[start..].find("/>") else {
            break;
        };
        let element = &rest[start..start + length + 2];
        let id = attribute_values(element, "r:id").into_iter().next();
        let dangling = match id {
            Some(id) => id.is_empty() || !known_ids.contains(&id),
            None => true,
        };
        output.push_str(&rest[..start]);
        if dangling {
            skipped.push(
                attribute_values(element, "name")
                    .into_iter()
                    .next()
                    .unwrap_or_default(),
            );
        } else {
            output.push_str(element);
        }
        rest = &rest[start + length + 2..];
    }
    output.push_str(rest);
    (output, skipped)
}

fn check_date_system(has_1904_epoch: bool) -> Result<(), ImportError> {
    if has_1904_epoch {
        Err(ImportError::UnsupportedDateSystem { observed: "1904" })
    } else {
        Ok(())
    }
}

fn hash_file(path: &Path) -> Result<String, ImportError> {
    let mut source = File::open(path).map_err(|error| ImportError::Open(error.to_string()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|error| ImportError::Read(error.to_string()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[derive(Default)]
struct PivotLoad {
    caches: Vec<PivotCache>,
    tables: Vec<PivotTable>,
}

struct PivotFieldDraft {
    field: PivotCacheField,
    database: bool,
}

enum RawPivotScalar {
    Blank,
    Number(f64),
    Text(String),
    Shared(usize),
}

/// Pivot tables and their caches, installed before formulas compile.
/// A cache part over the workbook size limit, or a cache larger than
/// `limits.max_cells`, is skipped rather than failing the import.
fn read_pivots(path: &Path, limits: ImportLimits) -> PivotLoad {
    let Ok(file) = File::open(path) else {
        return PivotLoad::default();
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return PivotLoad::default();
    };
    let Some(workbook) = read_optional_part(&mut archive, "xl/workbook.xml") else {
        return PivotLoad::default();
    };
    let relationships = parse_relationships(
        &read_optional_part(&mut archive, "xl/_rels/workbook.xml.rels").unwrap_or_default(),
    );
    let mut caches_by_part: HashMap<String, usize> = HashMap::new();
    let mut load = PivotLoad::default();
    for (sheet, rel_id) in sheet_relationship_ids(&workbook).into_iter().enumerate() {
        let Some(target) = relationships
            .iter()
            .find(|relationship| relationship.id == rel_id)
            .map(|relationship| relationship.target.as_str())
        else {
            continue;
        };
        let sheet_part = resolve_package_part("xl/workbook.xml", target);
        let rels =
            read_optional_part(&mut archive, &package_rels_path(&sheet_part)).unwrap_or_default();
        for relationship in parse_relationships(&rels) {
            if !relationship.kind.ends_with("/pivotTable") {
                continue;
            }
            let part = resolve_package_part(&sheet_part, &relationship.target);
            let Ok(xml) = read_part(&mut archive, &part) else {
                continue;
            };
            let Some(mut table) = parse_pivot_table(&xml) else {
                continue;
            };
            let table_rels =
                read_optional_part(&mut archive, &package_rels_path(&part)).unwrap_or_default();
            let Some(cache_relationship) = parse_relationships(&table_rels)
                .into_iter()
                .find(|relationship| relationship.kind.ends_with("/pivotCacheDefinition"))
            else {
                continue;
            };
            let cache_part = resolve_package_part(&part, &cache_relationship.target);
            let cache_index = if let Some(index) = caches_by_part.get(&cache_part).copied() {
                index
            } else {
                let Some(cache) = load_pivot_cache(&mut archive, &cache_part, limits) else {
                    continue;
                };
                let index = load.caches.len();
                load.caches.push(cache);
                caches_by_part.insert(cache_part, index);
                index
            };
            table.sheet = sheet as u32;
            table.cache = cache_index;
            load.tables.push(table);
        }
    }
    load
}

fn load_pivot_cache(
    archive: &mut zip::ZipArchive<File>,
    cache_part: &str,
    limits: ImportLimits,
) -> Option<PivotCache> {
    let definition = read_part(archive, cache_part).ok()?;
    let rels = read_optional_part(archive, &package_rels_path(cache_part)).unwrap_or_default();
    let records_relationship = parse_relationships(&rels)
        .into_iter()
        .find(|relationship| relationship.kind.ends_with("/pivotCacheRecords"))?;
    let records_part = resolve_package_part(cache_part, &records_relationship.target);
    let records = read_part(archive, &records_part).ok()?;
    parse_pivot_cache(&definition, &records, limits)
}

fn parse_pivot_cache(
    definition: &str,
    records_xml: &str,
    limits: ImportLimits,
) -> Option<PivotCache> {
    let mut drafts = Vec::new();
    scan_elements(definition, "cacheField", |tag, body| {
        let name = attribute(tag, "name").unwrap_or_default();
        let database = attribute(tag, "databaseField")
            .map(|value| value != "0")
            .unwrap_or(true);
        let mut shared = Vec::new();
        scan_elements(body, "sharedItems", |_tag, items| {
            if shared.is_empty() {
                shared = pivot_scalars(items);
            }
        });
        let mut group = None;
        scan_elements(body, "fieldGroup", |group_tag, group_body| {
            if group.is_none() {
                group = parse_date_group(group_tag, group_body);
            }
        });
        if shared.is_empty() {
            if let Some(group) = &group {
                shared = group.items.iter().cloned().map(PivotScalar::Text).collect();
            }
        }
        drafts.push(PivotFieldDraft {
            field: PivotCacheField {
                name,
                shared,
                group,
            },
            database,
        });
    });
    if drafts.is_empty() || drafts.len() > limits.max_cells {
        return None;
    }
    if drafts
        .iter()
        .any(|draft| draft.field.shared.len() > limits.max_cells)
    {
        return None;
    }
    let mut records = Vec::new();
    let mut overflow = false;
    let width = drafts.len();
    scan_elements(records_xml, "r", |_tag, body| {
        if records.len() >= limits.max_cells {
            overflow = true;
            return;
        }
        let children = raw_sequence(body);
        let mut record = vec![PivotScalar::Blank; width];
        let mut child = 0;
        for (index, draft) in drafts.iter().enumerate() {
            if !draft.database {
                continue;
            }
            if let Some(raw) = children.get(child) {
                record[index] = resolve_raw(raw, &draft.field.shared);
            }
            child += 1;
        }
        records.push(record);
    });
    if overflow || records.len().saturating_mul(width.max(1)) > limits.max_cells {
        return None;
    }
    Some(PivotCache {
        fields: drafts.into_iter().map(|draft| draft.field).collect(),
        records,
    })
}

fn parse_date_group(tag: &str, body: &str) -> Option<PivotDateGroup> {
    let base = attribute(tag, "base")?.parse().ok()?;
    let mut by = None;
    let mut start = None;
    let mut end = None;
    scan_elements(body, "rangePr", |range_tag, _| {
        // Day, hour, and numeric range groups are not calculated.
        by = attribute(range_tag, "groupBy").and_then(|value| match value.as_str() {
            "years" => Some(PivotGroupBy::Years),
            "quarters" => Some(PivotGroupBy::Quarters),
            "months" => Some(PivotGroupBy::Months),
            _ => None,
        });
        start = attribute(range_tag, "startDate").and_then(|value| cache_datetime_serial(&value));
        end = attribute(range_tag, "endDate").and_then(|value| cache_datetime_serial(&value));
    });
    let mut items = Vec::new();
    scan_elements(body, "groupItems", |_tag, items_body| {
        if items.is_empty() {
            for scalar in pivot_scalars(items_body) {
                match scalar {
                    PivotScalar::Text(text) => items.push(text),
                    PivotScalar::Number(number) => items.push(number.to_string()),
                    PivotScalar::Blank => items.push(String::new()),
                }
            }
        }
    });
    Some(PivotDateGroup {
        base,
        by: by?,
        start: start?,
        end: end?,
        items,
    })
}

fn parse_pivot_table(xml: &str) -> Option<PivotTable> {
    let mut location = None;
    scan_elements(xml, "location", |tag, _| {
        if location.is_none() {
            if let Some(reference) = attribute(tag, "ref") {
                location = parse_area(&reference);
            }
        }
    });
    let (first_row, first_column, last_row, last_column) = location?;
    let mut pivot_fields = Vec::new();
    scan_elements(xml, "pivotFields", |_tag, body| {
        if !pivot_fields.is_empty() {
            return;
        }
        scan_elements(body, "pivotField", |_tag, field_body| {
            let mut items = Vec::new();
            scan_elements(field_body, "item", |item_tag, _| {
                let shared = attribute(item_tag, "x").and_then(|value| value.parse().ok());
                let hidden = attribute(item_tag, "h")
                    .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
                items.push((shared, hidden));
            });
            pivot_fields.push(items);
        });
    });
    let mut axis_fields = Vec::new();
    for container in ["rowFields", "colFields"] {
        scan_elements(xml, container, |_tag, body| {
            scan_elements(body, "field", |tag, _| {
                if let Some(index) = attribute(tag, "x").and_then(|value| value.parse::<i32>().ok())
                {
                    if index >= 0 {
                        push_unique(&mut axis_fields, index as usize);
                    }
                }
            });
        });
    }
    let mut page_selection: Vec<(usize, usize)> = Vec::new();
    scan_elements(xml, "pageFields", |_tag, body| {
        scan_elements(body, "pageField", |tag, _| {
            let Some(field) = attribute(tag, "fld").and_then(|value| value.parse().ok()) else {
                return;
            };
            push_unique(&mut axis_fields, field);
            if let Some(item) = attribute(tag, "item").and_then(|value| value.parse().ok()) {
                page_selection.push((field, item));
            }
        });
    });
    let mut visible_items = Vec::new();
    for (field, item_index) in page_selection {
        let Some(shared) = pivot_fields
            .get(field)
            .and_then(|items| items.get(item_index))
            .and_then(|(shared, _)| *shared)
        else {
            continue;
        };
        visible_items.push((field, vec![shared]));
    }
    for (index, items) in pivot_fields.iter().enumerate() {
        if !axis_fields.contains(&index) || visible_items.iter().any(|(field, _)| *field == index) {
            continue;
        }
        if !items.iter().any(|(_, hidden)| *hidden) {
            continue;
        }
        let visible = items
            .iter()
            .filter(|(_, hidden)| !*hidden)
            .filter_map(|(shared, _)| *shared)
            .collect();
        visible_items.push((index, visible));
    }
    let mut data_fields = Vec::new();
    scan_elements(xml, "dataFields", |_tag, body| {
        if !data_fields.is_empty() {
            return;
        }
        scan_elements(body, "dataField", |tag, _| {
            let Some(name) = attribute(tag, "name") else {
                return;
            };
            let Some(source) = attribute(tag, "fld").and_then(|value| value.parse().ok()) else {
                return;
            };
            data_fields.push(PivotDataField {
                name,
                source,
                aggregate: pivot_aggregate(tag),
            });
        });
    });
    let mut filters = Vec::new();
    scan_elements(xml, "filters", |_tag, body| {
        scan_elements(body, "filter", |tag, filter_body| {
            if let Some(filter) = parse_date_filter(tag, filter_body) {
                filters.push(filter);
            }
        });
    });
    Some(PivotTable {
        sheet: 0,
        first_row,
        last_row,
        first_column,
        last_column,
        cache: 0,
        data_fields,
        axis_fields,
        filters,
        visible_items,
    })
}

fn pivot_aggregate(tag: &str) -> PivotAggregate {
    let summed = match attribute(tag, "subtotal").as_deref() {
        None => true,
        Some(value) => value.eq_ignore_ascii_case("sum"),
    };
    let normal = match attribute(tag, "showDataAs").as_deref() {
        None => true,
        Some(value) => value.eq_ignore_ascii_case("normal"),
    };
    if summed && normal {
        PivotAggregate::Sum
    } else {
        PivotAggregate::Unsupported
    }
}

/// `dateBetween` / `dateNotBetween` only. Relative filters such as `today`
/// need a clock and are left unset.
fn parse_date_filter(tag: &str, body: &str) -> Option<PivotDateFilter> {
    let negated = match attribute(tag, "type")?.as_str() {
        "dateBetween" => false,
        "dateNotBetween" => true,
        _ => return None,
    };
    let field = attribute(tag, "fld")?.parse().ok()?;
    let mut low = None;
    let mut high = None;
    scan_elements(body, "customFilter", |filter_tag, _| {
        let Some(operator) = attribute(filter_tag, "operator") else {
            return;
        };
        let Some(value) = attribute(filter_tag, "val").and_then(|text| text.parse::<f64>().ok())
        else {
            return;
        };
        if !value.is_finite() {
            return;
        }
        match operator.as_str() {
            "greaterThan" => low = Some((value, false)),
            "greaterThanOrEqual" => low = Some((value, true)),
            "lessThan" => high = Some((value, false)),
            "lessThanOrEqual" => high = Some((value, true)),
            "equal" => {
                low = Some((value, true));
                high = Some((value, true));
            }
            _ => {}
        }
    });
    let (low, low_inclusive) = low?;
    let (high, high_inclusive) = high?;
    Some(PivotDateFilter {
        field,
        low,
        high,
        low_inclusive,
        high_inclusive,
        negated,
    })
}

fn push_unique(values: &mut Vec<usize>, value: usize) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn pivot_scalars(xml: &str) -> Vec<PivotScalar> {
    raw_sequence(xml)
        .into_iter()
        .map(|raw| match raw {
            RawPivotScalar::Blank | RawPivotScalar::Shared(_) => PivotScalar::Blank,
            RawPivotScalar::Number(number) => PivotScalar::Number(number),
            RawPivotScalar::Text(text) => PivotScalar::Text(text),
        })
        .collect()
}

fn raw_sequence(xml: &str) -> Vec<RawPivotScalar> {
    let mut values = Vec::new();
    scan_top_level(xml, |name, attrs, _| {
        values.push(raw_scalar(name, attrs));
    });
    values
}

fn raw_scalar(name: &str, attrs: &str) -> RawPivotScalar {
    let value = attribute(attrs, "v");
    match name {
        "m" => RawPivotScalar::Blank,
        "n" => match value.as_deref().and_then(|text| text.parse::<f64>().ok()) {
            Some(number) if number.is_finite() => RawPivotScalar::Number(number),
            _ => RawPivotScalar::Blank,
        },
        "d" => match value.as_deref().and_then(cache_datetime_serial) {
            Some(serial) => RawPivotScalar::Number(serial),
            _ => RawPivotScalar::Blank,
        },
        "s" => RawPivotScalar::Text(value.unwrap_or_default()),
        "b" => {
            let truth = value
                .as_deref()
                .is_some_and(|text| text == "1" || text.eq_ignore_ascii_case("true"));
            RawPivotScalar::Number(if truth { 1.0 } else { 0.0 })
        }
        "e" => RawPivotScalar::Blank,
        "x" => value
            .as_deref()
            .and_then(|text| text.parse().ok())
            .map(RawPivotScalar::Shared)
            .unwrap_or(RawPivotScalar::Blank),
        _ => RawPivotScalar::Blank,
    }
}

fn resolve_raw(raw: &RawPivotScalar, shared: &[PivotScalar]) -> PivotScalar {
    match raw {
        RawPivotScalar::Blank => PivotScalar::Blank,
        RawPivotScalar::Number(number) => PivotScalar::Number(*number),
        RawPivotScalar::Text(text) => PivotScalar::Text(text.clone()),
        RawPivotScalar::Shared(index) => shared.get(*index).cloned().unwrap_or(PivotScalar::Blank),
    }
}

fn scan_top_level(xml: &str, mut visit: impl FnMut(&str, &str, &str)) {
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        if after.starts_with('/') || after.starts_with('!') || after.starts_with('?') {
            rest = after;
            continue;
        }
        let name_len = after
            .find(|character: char| !character.is_ascii_alphanumeric())
            .unwrap_or(after.len());
        if name_len == 0 {
            rest = after;
            continue;
        }
        let name = &after[..name_len];
        let after_name = &after[name_len..];
        if after_name.starts_with(':') {
            let Some(tag_end) = after_name.find('>') else {
                break;
            };
            rest = &after_name[tag_end + 1..];
            continue;
        }
        if !after_name.starts_with([' ', '>', '/', '\n', '\r', '\t']) && !after_name.is_empty() {
            rest = after;
            continue;
        }
        let Some(tag_end) = after_name.find('>') else {
            break;
        };
        let start_tag = &after_name[..tag_end];
        if start_tag.trim_end().ends_with('/') {
            visit(name, start_tag, "");
            rest = &after_name[tag_end + 1..];
            continue;
        }
        let content = &after_name[tag_end + 1..];
        let close = format!("</{name}>");
        let Some(end) = content.find(&close) else {
            break;
        };
        visit(name, start_tag, &content[..end]);
        rest = &content[end + close.len()..];
    }
}

#[cfg(test)]
fn import_ranges(
    ranges: Vec<(String, Range<Data>, Range<String>)>,
    source_sha256: String,
    limits: ImportLimits,
) -> Result<ImportedWorkbook, ImportError> {
    import_ranges_with_names(
        ranges,
        Vec::new(),
        &[],
        Vec::new(),
        Vec::new(),
        &HashMap::new(),
        PivotLoad::default(),
        source_sha256,
        limits,
    )
}

fn import_ranges_with_names(
    ranges: Vec<(String, Range<Data>, Range<String>)>,
    defined_names: Vec<DefinedName>,
    tables: &[StructuredTable],
    external_cells: Vec<CachedExternalCell>,
    external_sheets: Vec<ExternalSheetNote>,
    array_formulas: &HashMap<(u32, u32, u32), (usize, usize)>,
    pivots: PivotLoad,
    source_sha256: String,
    limits: ImportLimits,
) -> Result<ImportedWorkbook, ImportError> {
    if ranges.len() > limits.max_sheets {
        return Err(ImportError::TooManySheets {
            observed: ranges.len(),
            maximum: limits.max_sheets,
        });
    }
    let mut observed_cells = 0_usize;
    let mut observed_formulas = 0_usize;
    for (_, values, formulas) in &ranges {
        observed_cells =
            observed_cells.saturating_add(values.width().saturating_mul(values.height()));
        observed_formulas = observed_formulas.saturating_add(
            formulas
                .used_cells()
                .filter(|(_, _, formula)| !formula.is_empty())
                .count(),
        );
    }
    if observed_cells > limits.max_cells {
        return Err(ImportError::TooManyCells {
            observed: observed_cells,
            maximum: limits.max_cells,
        });
    }
    if observed_formulas > limits.max_formulas {
        return Err(ImportError::TooManyFormulas {
            observed: observed_formulas,
            maximum: limits.max_formulas,
        });
    }

    let sheets: Vec<SheetInfo> = ranges
        .iter()
        .enumerate()
        .map(|(index, (name, values, formulas))| {
            let (value_rows, value_columns) = range_extent(values.end());
            let (formula_rows, formula_columns) = range_extent(formulas.end());
            SheetInfo {
                index: index as u32,
                name: name.clone(),
                rows: value_rows.max(formula_rows),
                columns: value_columns.max(formula_columns),
            }
        })
        .collect();
    let mut workbook = Workbook::default();
    // One recalculation for the whole import instead of one per cell.
    workbook.begin_bulk();
    for sheet in &sheets {
        workbook.define_sheet(sheet.index, sheet.name.clone());
    }
    let sheet_indices: HashMap<&str, u32> = sheets
        .iter()
        .map(|sheet| (sheet.name.as_str(), sheet.index))
        .collect();
    for name in defined_names {
        match name.sheet {
            None => workbook.define_name(name.name, name.definition),
            // A scope naming a sheet the repair skipped goes with that sheet.
            Some(scope) => {
                if let Some(index) = sheet_indices.get(scope.as_str()) {
                    workbook.define_sheet_name(*index, name.name, name.definition);
                }
            }
        }
    }
    for sheet in external_sheets {
        if sheet.broken {
            workbook.note_broken_external_sheet(
                sheet.link_index,
                sheet.book_file.as_deref(),
                &sheet.sheet,
            );
        } else {
            workbook.note_external_sheet(
                sheet.link_index,
                sheet.book_file.as_deref(),
                &sheet.sheet,
            );
        }
    }
    for external in external_cells {
        workbook.cache_external_cell(
            external.link_index,
            external.book_file.as_deref(),
            &external.sheet,
            external.row,
            external.column,
            external.value,
        );
    }
    for cache in pivots.caches {
        workbook.add_pivot_cache(cache);
    }
    for table in pivots.tables {
        workbook.add_pivot_table(table);
    }
    let mut source_cells = BTreeMap::new();

    for (sheet, (_, values, formulas)) in ranges.into_iter().enumerate() {
        let (value_row, value_column) = values.start().unwrap_or((0, 0));
        for (row, column, value) in values.used_cells() {
            let cell = CellId::new(
                sheet as u32,
                value_row + row as u32,
                value_column + column as u32,
            );
            set_source_value(&mut workbook, cell, value);
            source_cells.insert(
                cell,
                ImportedCell {
                    cell,
                    stored: source_value(value),
                    formula: None,
                },
            );
        }
        let (formula_row, formula_column) = formulas.start().unwrap_or((0, 0));
        for (row, column, formula) in formulas.used_cells() {
            if formula.is_empty() {
                continue;
            }
            let absolute = (formula_row + row as u32, formula_column + column as u32);
            let cell = CellId::new(sheet as u32, absolute.0, absolute.1);
            source_cells
                .entry(cell)
                .or_insert_with(|| ImportedCell {
                    cell,
                    stored: values
                        .get_value(absolute)
                        .map(source_value)
                        .unwrap_or(Value::Blank),
                    formula: None,
                })
                .formula = Some(formula.clone());
        }
    }

    let source_cells: Vec<_> = source_cells.into_values().collect();
    if let Some(at) =
        tick_from_cached_volatile(&source_cells).or_else(|| infer_yearfrac_today(&source_cells))
    {
        // Before formulas are installed, so TODAY() and NOW() replay this
        // serial instead of staying #N/A. Does not read the system clock.
        workbook.set_tick(at);
    }
    let mut unsupported = Vec::new();
    let mut compiled_cells = Vec::with_capacity(observed_formulas);
    for (index, source) in source_cells.iter().enumerate() {
        let Some(formula) = &source.formula else {
            continue;
        };
        let cell = source.cell;
        if let Some(&(rows, columns)) = array_formulas.get(&(cell.sheet, cell.row, cell.column)) {
            workbook.note_array_formula(cell, rows, columns);
        }
        match workbook.set_formula_with_tables(cell, formula, tables) {
            Ok(_) => compiled_cells.push(index),
            Err(error) => unsupported.push(UnsupportedFormula {
                cell,
                reason: bounded_formula_error(&error),
                error,
            }),
        }
    }
    let formula_cells_loaded = compiled_cells.len();
    workbook.end_bulk();
    Ok(ImportedWorkbook {
        workbook,
        sheets,
        source_sha256,
        date_system: DATE_SYSTEM,
        unsupported,
        skipped_sheets: Vec::new(),
        source_cells,
        compiled_cells,
        formula_cells_observed: observed_formulas,
        formula_cells_loaded,
    })
}

fn read_tables(path: &Path) -> Result<Vec<StructuredTable>, ImportError> {
    let file = File::open(path).map_err(|error| ImportError::Open(error.to_string()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| ImportError::Open(error.to_string()))?;
    let workbook = read_part(&mut archive, "xl/workbook.xml")?;
    let relationships = parse_relationships(
        &read_optional_part(&mut archive, "xl/_rels/workbook.xml.rels").unwrap_or_default(),
    );
    let mut tables = Vec::new();
    for (sheet, rel_id) in sheet_relationship_ids(&workbook).into_iter().enumerate() {
        let Some(target) = relationships
            .iter()
            .find(|relationship| relationship.id == rel_id)
            .map(|relationship| relationship.target.as_str())
        else {
            continue;
        };
        let sheet_part = resolve_package_part("xl/workbook.xml", target);
        let rels =
            read_optional_part(&mut archive, &package_rels_path(&sheet_part)).unwrap_or_default();
        for relationship in parse_relationships(&rels) {
            if !relationship.kind.ends_with("/table") {
                continue;
            }
            let part = resolve_package_part(&sheet_part, &relationship.target);
            let Ok(xml) = read_part(&mut archive, &part) else {
                continue;
            };
            if let Some(table) = parse_table(&xml, sheet as u32) {
                tables.push(table);
            }
        }
    }
    Ok(tables)
}

/// Legacy CSE anchors: `(sheet, row, column) -> (rows, columns)` of the
/// entered rectangle. The formula text stays on the anchor cell.
fn read_array_formulas(
    path: &Path,
) -> Result<HashMap<(u32, u32, u32), (usize, usize)>, ImportError> {
    let mut archive = zip::ZipArchive::new(
        File::open(path).map_err(|error| ImportError::Open(error.to_string()))?,
    )
    .map_err(|error| ImportError::Open(error.to_string()))?;
    let workbook = read_part(&mut archive, "xl/workbook.xml")?;
    let relationships = parse_relationships(
        &read_optional_part(&mut archive, "xl/_rels/workbook.xml.rels").unwrap_or_default(),
    );
    let mut anchors = HashMap::new();
    for (sheet, rel_id) in sheet_relationship_ids(&workbook).into_iter().enumerate() {
        let Some(target) = relationships
            .iter()
            .find(|relationship| relationship.id == rel_id)
            .map(|relationship| relationship.target.as_str())
        else {
            continue;
        };
        let sheet_part = resolve_package_part("xl/workbook.xml", target);
        let Ok(xml) = read_part(&mut archive, &sheet_part) else {
            continue;
        };
        if !xml.contains("t=\"array\"") {
            continue;
        }
        for (row, column, rows, columns) in array_anchors(&xml) {
            anchors.insert((sheet as u32, row, column), (rows, columns));
        }
    }
    Ok(anchors)
}

fn array_anchors(xml: &str) -> Vec<(u32, u32, usize, usize)> {
    let mut anchors = Vec::new();
    scan_elements(xml, "c", |tag, body| {
        let Some(reference) = attribute(tag, "r") else {
            return;
        };
        let Some((row, column)) = parse_cell_reference(&reference) else {
            return;
        };
        scan_elements(body, "f", |formula_tag, _| {
            if attribute(formula_tag, "t").as_deref() != Some("array") {
                return;
            }
            let Some(span) = attribute(formula_tag, "ref") else {
                return;
            };
            let Some((row0, column0, row1, column1)) = parse_area(&span) else {
                return;
            };
            if row0 != row || column0 != column || row1 < row0 || column1 < column0 {
                return;
            }
            anchors.push((
                row,
                column,
                (row1 - row0 + 1) as usize,
                (column1 - column0 + 1) as usize,
            ));
        });
    });
    anchors
}

fn sheet_relationship_ids(workbook_xml: &str) -> Vec<String> {
    let mut ids = Vec::new();
    scan_elements(workbook_xml, "sheet", |tag, _| {
        if let Some(id) = attribute(tag, "r:id").or_else(|| attribute(tag, "id")) {
            ids.push(id);
        }
    });
    ids
}

fn parse_table(xml: &str, sheet: u32) -> Option<StructuredTable> {
    let mut parsed = None;
    scan_elements(xml, "table", |tag, body| {
        let Some(name) = attribute(tag, "name").or_else(|| attribute(tag, "displayName")) else {
            return;
        };
        let Some(reference) = attribute(tag, "ref") else {
            return;
        };
        let Some((row0, column0, row1, _)) = parse_area(&reference) else {
            return;
        };
        let header_count = attribute(tag, "headerRowCount")
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(1);
        let totals_count = attribute(tag, "totalsRowCount")
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        let mut columns = Vec::new();
        scan_elements(body, "tableColumn", |column_tag, _| {
            if let Some(column_name) = attribute(column_tag, "name") {
                let column = column0 + columns.len() as u32;
                columns.push(StructuredColumn {
                    name: column_name,
                    column,
                });
            }
        });
        if columns.is_empty() {
            return;
        }
        let header_row = (header_count > 0).then_some(row0);
        let totals_row = (totals_count > 0).then_some(row1);
        let first_data = row0.saturating_add(header_count);
        let last_data = row1.saturating_sub(totals_count);
        let rows = if first_data <= last_data {
            (first_data..=last_data).collect()
        } else {
            Vec::new()
        };
        parsed = Some(StructuredTable {
            name,
            sheet,
            header_row,
            totals_row,
            rows,
            columns,
        });
    });
    parsed
}

fn parse_area(reference: &str) -> Option<(u32, u32, u32, u32)> {
    let (start, end) = reference.split_once(':').unwrap_or((reference, reference));
    let (row0, column0) = parse_cell_reference(start)?;
    let (row1, column1) = parse_cell_reference(end)?;
    Some((
        row0.min(row1),
        column0.min(column1),
        row0.max(row1),
        column0.max(column1),
    ))
}

/// Serial for `YEARFRAC(TODAY(), date, basis)` when the cell stores the
/// formula result and the other arguments are numbers. One serial has to
/// satisfy every such formula. Does not read the system clock.
fn infer_yearfrac_today(cells: &[ImportedCell]) -> Option<i64> {
    let numbers: HashMap<(u32, u32, u32), f64> = cells
        .iter()
        .filter_map(|cell| match cell.stored {
            Value::Number(number) if number.is_finite() => {
                Some(((cell.cell.sheet, cell.cell.row, cell.cell.column), number))
            }
            _ => None,
        })
        .collect();
    let probes: Vec<YearFracProbe> = cells
        .iter()
        .filter_map(|cell| yearfrac_probe(cell, &numbers))
        .collect();
    if probes.is_empty() {
        return None;
    }
    let mut candidates = Vec::new();
    for probe in &probes {
        let Some(fraction) = implied_year_fraction(probe) else {
            continue;
        };
        let year = match probe.basis {
            0 | 2 | 4 => 360.0,
            3 => 365.0,
            _ => 365.25,
        };
        let guess = probe.end_serial + (fraction * year).round() as i64;
        for delta in -20..=20 {
            let serial = guess + delta;
            if (1..=60_000).contains(&serial) {
                candidates.push(serial);
            }
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    let mut best_serial = None;
    let mut best_error = f64::MAX;
    for serial in candidates {
        let Some(error) = yearfrac_probe_error(serial, &probes) else {
            continue;
        };
        if error < best_error {
            best_error = error;
            best_serial = Some(serial);
        }
    }
    let serial = best_serial?;
    let acceptable = probes.iter().all(|probe| {
        let Ok(fraction) =
            omasheets_calc::serial_date::year_fraction(serial, probe.end_serial, probe.basis)
        else {
            return false;
        };
        let predicted = match probe.rate {
            Some(rate) => (1.0 + rate).powf(-fraction),
            None => fraction,
        };
        (predicted - probe.stored).abs() <= 1e-9 * probe.stored.abs().max(1.0)
    });
    if !acceptable {
        return None;
    }
    omasheets_calc::serial_date::unix_millis_from_serial(serial as f64).ok()
}

struct YearFracProbe {
    end_serial: i64,
    basis: i64,
    rate: Option<f64>,
    stored: f64,
}

fn implied_year_fraction(probe: &YearFracProbe) -> Option<f64> {
    match probe.rate {
        Some(rate) => {
            let base = 1.0 + rate;
            if probe.stored <= 0.0 || base <= 0.0 || base == 1.0 {
                return None;
            }
            Some(-probe.stored.ln() / base.ln())
        }
        None => Some(probe.stored),
    }
}

fn yearfrac_probe_error(serial: i64, probes: &[YearFracProbe]) -> Option<f64> {
    let mut error = 0.0;
    for probe in probes {
        let fraction =
            omasheets_calc::serial_date::year_fraction(serial, probe.end_serial, probe.basis)
                .ok()?;
        let predicted = match probe.rate {
            Some(rate) => (1.0 + rate).powf(-fraction),
            None => fraction,
        };
        error += (predicted - probe.stored).abs();
    }
    Some(error)
}

fn yearfrac_probe(
    cell: &ImportedCell,
    numbers: &HashMap<(u32, u32, u32), f64>,
) -> Option<YearFracProbe> {
    let Value::Number(stored) = cell.stored else {
        return None;
    };
    let formula = cell.formula.as_deref()?;
    let mut text = formula.trim();
    if let Some(rest) = text.strip_prefix('=') {
        text = rest.trim_start();
    }
    let mut compact: String = text
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '$')
        .collect();
    while compact.starts_with('(') && compact.ends_with(')') && compact.len() > 1 {
        compact.remove(0);
        compact.pop();
    }
    let upper = compact.to_ascii_uppercase();
    let (rate_ref, body) = if let Some(rest) = upper.strip_prefix("1/(1+") {
        let (rate, after) = rest.split_once(")^")?;
        (Some(rate.to_string()), after.to_string())
    } else {
        (None, upper.clone())
    };
    let args = body.strip_prefix("YEARFRAC(")?.strip_suffix(')')?;
    let parts: Vec<&str> = args.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    let basis = parts[2].parse::<i64>().ok()?;
    let end_ref = if parts[0] == "TODAY()" {
        parts[1]
    } else if parts[1] == "TODAY()" {
        parts[0]
    } else {
        return None;
    };
    let (end_row, end_column) = parse_cell_reference(end_ref)?;
    let end = numbers.get(&(cell.cell.sheet, end_row, end_column))?;
    let end_serial = omasheets_calc::serial_date::serial_from_number(*end).ok()?;
    let rate = match rate_ref {
        Some(reference) => {
            let (row, column) = parse_cell_reference(&reference)?;
            Some(*numbers.get(&(cell.cell.sheet, row, column))?)
        }
        None => None,
    };
    Some(YearFracProbe {
        end_serial,
        basis,
        rate,
        stored,
    })
}

/// First cached `NOW()` serial, else the first cached `TODAY()` serial, as
/// UTC Unix milliseconds. Anything that is not exactly that call is ignored,
/// so `TODAY()+1` cannot move the tick. No cached volatile leaves the
/// workbook without a tick.
fn tick_from_cached_volatile(cells: &[ImportedCell]) -> Option<i64> {
    let mut today = None;
    let mut now = None;
    for cell in cells {
        let Some(formula) = cell.formula.as_deref() else {
            continue;
        };
        let Value::Number(serial) = cell.stored else {
            continue;
        };
        match cached_clock_formula(formula) {
            Some(true) if now.is_none() => now = Some(serial),
            Some(false) if today.is_none() => today = Some(serial),
            _ => {}
        }
    }
    for serial in [now, today].into_iter().flatten() {
        if let Ok(millis) = omasheets_calc::serial_date::unix_millis_from_serial(serial) {
            return Some(millis);
        }
    }
    None
}

/// `Some(true)` is `NOW()`, `Some(false)` is `TODAY()`. Optional `=` and
/// unary `+`, plus whitespace, are ignored. Any other spelling is not a tick.
fn cached_clock_formula(formula: &str) -> Option<bool> {
    let mut text = formula.trim();
    if let Some(rest) = text.strip_prefix('=') {
        text = rest.trim_start();
    }
    while text.starts_with('+') {
        text = text[1..].trim_start();
    }
    let compact: String = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if compact.eq_ignore_ascii_case("NOW()") {
        Some(true)
    } else if compact.eq_ignore_ascii_case("TODAY()") {
        Some(false)
    } else {
        None
    }
}

fn range_extent(end: Option<(u32, u32)>) -> (usize, usize) {
    end.map(|(row, column)| (row as usize + 1, column as usize + 1))
        .unwrap_or((0, 0))
}

fn set_source_value(workbook: &mut Workbook, cell: CellId, value: &Data) {
    match value {
        Data::Int(value) => {
            workbook.set_number(cell, *value as f64);
        }
        Data::Float(value) => {
            workbook.set_number(cell, *value);
        }
        Data::Bool(value) => {
            workbook.set_boolean(cell, *value);
        }
        Data::String(value) | Data::DateTimeIso(value) | Data::DurationIso(value) => {
            workbook.set_text(cell, value.clone());
        }
        Data::DateTime(value) => {
            // The raw 1900-system serial; `check_date_system` has already
            // rejected 1904 workbooks, so no epoch shift is applied.
            workbook.set_number(cell, value.as_f64());
        }
        Data::Error(error) => {
            workbook.set_error(cell, source_error(error));
        }
        Data::Empty => {
            workbook.clear(cell);
        }
    }
}

fn source_error(error: &CellErrorType) -> CalcError {
    match error {
        CellErrorType::Div0 => CalcError::DivisionByZero,
        CellErrorType::NA => CalcError::NotAvailable,
        CellErrorType::Name => CalcError::InvalidName,
        CellErrorType::Null => CalcError::NullIntersection,
        CellErrorType::Num => CalcError::InvalidNumber,
        CellErrorType::Ref => CalcError::InvalidReference,
        CellErrorType::Value | CellErrorType::GettingData => CalcError::InvalidValue,
    }
}

fn source_value(value: &Data) -> Value {
    match value {
        Data::Int(value) => Value::Number(*value as f64),
        Data::Float(value) => Value::Number(*value),
        Data::Bool(value) => Value::Boolean(*value),
        Data::String(value) | Data::DateTimeIso(value) | Data::DurationIso(value) => {
            Value::Text(value.clone())
        }
        Data::DateTime(value) => Value::Number(value.as_f64()),
        Data::Error(error) => Value::Error(source_error(error)),
        Data::Empty => Value::Blank,
    }
}

fn bounded_formula_error(error: &FormulaError) -> String {
    error.to_string().chars().take(256).collect()
}

fn value_kind(value: &Value) -> &str {
    match value {
        Value::Blank => "blank",
        Value::Number(_) => "number",
        Value::Boolean(_) => "boolean",
        Value::Text(_) => "text",
        Value::Error(error) => error.label(),
    }
}

fn values_match(stored: &Value, calculated: &Value) -> bool {
    match (stored, calculated) {
        (Value::Number(stored), Value::Number(calculated)) => {
            stored.is_finite()
                && calculated.is_finite()
                && (stored - calculated).abs() <= 1e-9 * stored.abs().max(1.0)
        }
        _ => stored == calculated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workbook_name(name: &str, definition: &str) -> DefinedName {
        DefinedName {
            sheet: None,
            name: name.into(),
            definition: definition.into(),
        }
    }

    /// A minimal package: one real worksheet plus `<sheet>` entries that
    /// point nowhere, the shape left behind by converted legacy macro sheets.
    fn package_with_dangling_sheets(dangling: &[(&str, &str)]) -> Vec<u8> {
        package(
            dangling,
            r#"<definedName name="Total">Data!$A$3</definedName>"#,
            "",
            "",
        )
    }

    /// A minimal package with the given dangling `<sheet>` entries, the given
    /// `<definedNames>` body, extra `<c>` cells appended to row 1 and extra
    /// `<row>` elements appended after row 3.
    fn package(
        dangling: &[(&str, &str)],
        defined_names: &str,
        extra_cells: &str,
        extra_rows: &str,
    ) -> Vec<u8> {
        package_with_chartsheet(dangling, defined_names, extra_cells, extra_rows, false)
    }

    /// As [`package`], optionally with a chartsheet named `Chart` after the
    /// worksheet.
    fn package_with_chartsheet(
        dangling: &[(&str, &str)],
        defined_names: &str,
        extra_cells: &str,
        extra_rows: &str,
        chartsheet: bool,
    ) -> Vec<u8> {
        let chart_sheet_entry = if chartsheet {
            r#"<sheet name="Chart" sheetId="2" r:id="rId2"/>"#
        } else {
            ""
        };
        let chart_relationship = if chartsheet {
            r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.xml"/>"#
        } else {
            ""
        };
        let chart_override = if chartsheet {
            r#"<Override PartName="/xl/chartsheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.chartsheet+xml"/>"#
        } else {
            ""
        };
        let sheets: String = dangling
            .iter()
            .map(|(name, id)| {
                format!(r#"<sheet name="{name}" sheetId="9" state="veryHidden" r:id="{id}"/>"#)
            })
            .collect();
        let parts = [
            (
                "[Content_Types].xml",
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>{chart_override}</Types>"#
                ),
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_string(),
            ),
            (
                "xl/workbook.xml",
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/>{chart_sheet_entry}{sheets}</sheets><definedNames>{defined_names}</definedNames></workbook>"#
                ),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>{chart_relationship}</Relationships>"#
                ),
            ),
            (
                "xl/chartsheets/sheet1.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><chartsheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetPr/><sheetViews><sheetView workbookViewId="0"/></sheetViews></chartsheet>"#.to_string(),
            ),
            (
                "xl/worksheets/sheet1.xml",
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>2</v></c>{extra_cells}</row><row r="2"><c r="A2"><v>3</v></c></row><row r="3"><c r="A3"><f>A1+A2</f><v>5</v></c></row>{extra_rows}</sheetData></worksheet>"#
                ),
            ),
        ];
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in parts {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn temporary_xlsx(bytes: &[u8]) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "omasheets-xlsx-{}-{nonce}.xlsx",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn defined_names_keep_their_sheet_scope_and_entities() {
        let xml = r#"<definedName name="Total">Data!$A$3</definedName><definedName name="Total" localSheetId="0" hidden="1">Data!$A$1</definedName><definedName name="Joined" comment="a &amp; b">Data!$A$1&amp;"x"</definedName><definedName name="Orphan" localSheetId="7">Data!$A$1</definedName><definedName name="Empty"/><definedName name="Quoted">'P &amp; L'!$B$2</definedName>"#;
        assert_eq!(
            parse_defined_names(&format!(
                r#"<workbook><sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="P &amp; L" sheetId="2" r:id="rId2"/></sheets><definedNames>{xml}</definedNames></workbook>"#
            )),
            vec![
                workbook_name("Total", "Data!$A$3"),
                DefinedName {
                    sheet: Some("Data".into()),
                    name: "Total".into(),
                    definition: "Data!$A$1".into(),
                },
                workbook_name("Joined", "Data!$A$1&\"x\""),
                workbook_name("Empty", ""),
                workbook_name("Quoted", "'P & L'!$B$2"),
            ]
        );
        assert_eq!(unescape_xml("&lt;&#65;&#x42;&bogus;&amp"), "<AB&bogus;&amp");

        // On the sheet, the scoped `Total` (A1 = 2) wins over the workbook
        // `Total` (A3 = 5): B1 stores 20, as Excel computed it.
        let path = temporary_xlsx(&package(
            &[],
            xml,
            r#"<c r="B1"><f>Total*10</f><v>20</v></c><c r="C1" t="str"><f>Joined</f><v>2x</v></c>"#,
            "",
        ));
        let imported = import_xlsx(&path, ImportLimits::default()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 1)),
            Value::Number(20.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 2)),
            Value::Text("2x".into())
        );
        assert_eq!(imported.parity().stored_values_matched, 3);
        assert_eq!(imported.parity().stored_values_mismatched, 0);
        assert_eq!(
            (imported.sheets[0].rows, imported.sheets[0].columns),
            (3, 3)
        );
        assert_eq!(imported.source_cells().len(), 5);
        let formula = imported
            .source_cells()
            .iter()
            .find(|source| source.cell == CellId::new(0, 2, 0))
            .unwrap();
        assert_eq!(formula.stored, Value::Number(5.0));
        assert_eq!(formula.formula.as_deref(), Some("A1+A2"));
    }

    #[test]
    fn reference_diagnostics_expose_only_fixed_failure_classes() {
        let bytes = package(
            &[],
            "",
            r#"<c r="B1"><f>SUM(A1:2)</f><v>0</v></c><c r="C1"><f>SUM(Data!A)</f><v>0</v></c><c r="D1"><f>SUM(A1:SUM(A1:A2))</f><v>0</v></c><c r="E1"><f>1+1</f><v>9</v></c>"#,
            "",
        );
        let path = temporary_xlsx(&bytes);
        let report = import_xlsx(&path, ImportLimits::default())
            .unwrap()
            .report();
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            report.reference_failure_kinds,
            BTreeMap::from([
                ("column_without_row".into(), 1),
                ("dynamic_range_endpoint".into(), 1),
                ("numeric_range_endpoint".into(), 1),
            ])
        );
        assert_eq!(
            report.mismatch_value_kinds,
            BTreeMap::from([("number -> number".into(), 1)])
        );
    }

    #[test]
    fn syntax_diagnostics_expose_only_fixed_token_classes() {
        let path = temporary_xlsx(&package(
            &[],
            "",
            r#"<c r="B1"><f>SUM(1;2)</f><v>3</v></c><c r="C1"><f>SUM({1,})</f><v>1</v></c>"#,
            "",
        ));
        let imported = import_xlsx(&path, ImportLimits::default()).unwrap();
        std::fs::remove_file(path).unwrap();
        let report = imported.report();
        assert_eq!(
            report.syntax_failure_tokens,
            BTreeMap::from([("semicolon".into(), 1), ("array_brace".into(), 1),])
        );
        assert_eq!(report.unsupported_reasons["syntax"], 2);
        assert_eq!(report.formula_cells_compared, 1);
    }

    #[test]
    fn matrix_and_database_formulas_match_independent_caches() {
        let path = temporary_xlsx(&package(
            &[],
            "",
            r#"<c r="B1"><f>MMULT(TRANSPOSE(A1:A2),A1:A2)</f><v>13</v></c><c r="C1"><f>INDEX(TRANSPOSE({1,2,3;4,5,6}),3,2)</f><v>6</v></c><c r="D1"><f>DAVERAGE({"Kind","Value";"A",10;"B",50;"A",20},"Value",{"Kind";"=A"})</f><v>15</v></c><c r="E1"><f>DMAX({"Kind","Value";"A",10;"B",50;"A",20},2,{"Kind";"=A"})</f><v>20</v></c><c r="F1"><f>DMIN({"Kind","Value";"A",10;"B",50;"A",20},2,{"Kind";"=A"})</f><v>10</v></c><c r="G1"><f>DSTDEV({"Kind","Value";"A",10;"B",50;"A",20},2,{"Kind";"=A"})</f><v>7.0710678118654755</v></c>"#,
            "",
        ));
        let report = import_xlsx(&path, ImportLimits::default())
            .unwrap()
            .report();
        std::fs::remove_file(path).unwrap();
        assert_eq!(report.formula_cells_loaded, 7);
        assert_eq!(report.stored_values_matched, 7);
        assert_eq!(report.stored_values_mismatched, 0);
    }

    #[test]
    fn reference_valued_index_matches_xlsx_caches() {
        let bytes = package(
            &[],
            "",
            r#"<c r="B1"><f>SUM(A1:INDEX(A1:A2,2))</f><v>5</v></c><c r="C1"><f>SUM(INDEX(A1:A2,0))</f><v>5</v></c><c r="D1"><f>MATCH(3,INDEX(A1:A2,0),0)</f><v>2</v></c>"#,
            "",
        );
        let path = temporary_xlsx(&bytes);
        let report = import_xlsx(&path, ImportLimits::default())
            .unwrap()
            .report();
        std::fs::remove_file(path).unwrap();
        assert_eq!(report.formula_cells_loaded, 4);
        assert_eq!(report.stored_values_matched, 4);
        assert_eq!(report.stored_values_mismatched, 0);
    }

    #[test]
    fn array_financial_and_deleted_reference_formulas_match_xlsx_caches() {
        let path = temporary_xlsx(&package(
            &[],
            r#"<definedName name="weights">{2;3}</definedName><definedName name="rate" localSheetId="0">0.1</definedName>"#,
            r#"<c r="B1"><f>SUMPRODUCT({10;20},weights)</f><v>80</v></c><c r="C1"><f>PV(Data!rate,1,-110)</f><v>100</v></c><c r="D1"><f>_xlfn.XLOOKUP(2,{1,2},{10,20})</f><v>20</v></c><c r="E1" t="e"><f>SUM(#REF!:#REF!)</f><v>#REF!</v></c><c r="F1"><f>IFERROR(SUM(A1:#REF!),17)</f><v>17</v></c>"#,
            "",
        ));
        let imported = import_xlsx(&path, ImportLimits::default()).unwrap();
        std::fs::remove_file(path).unwrap();
        let report = imported.report();
        assert_eq!(report.formula_cells_observed, 6);
        assert_eq!(report.formula_cells_loaded, 6);
        assert_eq!(report.stored_values_matched, 6);
        assert_eq!(report.stored_values_mismatched, 0);
        assert!(report.unsupported_reasons.is_empty());
    }

    #[test]
    fn shared_formulas_expand_from_their_anchor_cell() {
        // The shared group is anchored at B5 with ref A5:B6; A5 carries its
        // own formula. A6 is therefore B5's template shifted one row down
        // and one column left (A5*2 = 140), not the ref corner's (B5*2 = 44).
        let rows = r#"<row r="4"><c r="A4"><v>7</v></c><c r="B4"><v>11</v></c></row><row r="5"><c r="A5"><f>A4*10</f><v>70</v></c><c r="B5"><f t="shared" ref="A5:B6" si="0">B4*2</f><v>22</v></c></row><row r="6"><c r="A6"><f t="shared" si="0"/><v>140</v></c><c r="B6"><f t="shared" si="0"/><v>44</v></c></row>"#;
        let path = temporary_xlsx(&package(
            &[],
            r#"<definedName name="Total">Data!$A$3</definedName>"#,
            "",
            rows,
        ));
        let imported = import_xlsx(&path, ImportLimits::default()).unwrap();
        std::fs::remove_file(&path).unwrap();
        for (cell, expected) in [
            (CellId::new(0, 4, 0), 70.0),
            (CellId::new(0, 4, 1), 22.0),
            (CellId::new(0, 5, 0), 140.0),
            (CellId::new(0, 5, 1), 44.0),
        ] {
            assert_eq!(imported.workbook.value(cell), Value::Number(expected));
        }
        assert_eq!(imported.parity().formula_cells_loaded, 5);
        assert_eq!(imported.parity().stored_values_matched, 5);
        assert_eq!(imported.parity().stored_values_mismatched, 0);
    }

    #[test]
    fn chartsheets_import_as_empty_sheets() {
        let path = temporary_xlsx(&package_with_chartsheet(
            &[],
            r#"<definedName name="Total">Data!$A$3</definedName>"#,
            "",
            "",
            true,
        ));
        let imported = import_xlsx(&path, ImportLimits::default()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(imported.sheets.len(), 2);
        assert_eq!(imported.sheets[1].name, "Chart");
        assert_eq!(imported.parity().formula_cells_loaded, 1);
        assert_eq!(imported.parity().stored_values_matched, 1);
    }

    #[test]
    fn dangling_sheet_entries_are_skipped_in_memory_and_reported() {
        let path = temporary_xlsx(&package_with_dangling_sheets(&[
            ("Module1", ""),
            ("Code", "rId7"),
        ]));
        let imported = import_xlsx(&path, ImportLimits::default()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(imported.sheets.len(), 1);
        assert_eq!(imported.skipped_sheets, vec!["Module1", "Code"]);
        assert_eq!(
            imported.workbook.value(CellId::new(0, 2, 0)),
            Value::Number(5.0)
        );
        let report = imported.report();
        assert_eq!(report.skipped_sheets, vec!["Module1", "Code"]);
        assert_eq!(report.stored_values_matched, 1);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"skipped_sheets\":[\"Module1\",\"Code\"]"));
    }

    #[test]
    fn well_formed_packages_are_not_rewritten() {
        let path = temporary_xlsx(&package_with_dangling_sheets(&[]));
        let imported = import_xlsx(&path, ImportLimits::default()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(imported.skipped_sheets.is_empty());
        assert_eq!(imported.report().skipped_sheets, Vec::<String>::new());
    }

    #[test]
    fn dropping_dangling_sheets_keeps_every_other_byte() {
        let known: std::collections::HashSet<String> = ["rId1".to_string()].into_iter().collect();
        let xml = r#"<sheets><sheet name="A" sheetId="1" r:id="rId1"/><sheet name="M" sheetId="2" state="veryHidden" r:id=""/><sheet name="N" sheetId="3" r:id="rId9"/></sheets><definedNames/>"#;
        let (rewritten, skipped) = drop_dangling_sheets(xml, &known);
        assert_eq!(skipped, vec!["M", "N"]);
        assert_eq!(
            rewritten,
            r#"<sheets><sheet name="A" sheetId="1" r:id="rId1"/></sheets><definedNames/>"#
        );
        assert_eq!(attribute_values(xml, "name"), vec!["A", "M", "N"]);
        let (unchanged, none) =
            drop_dangling_sheets(r#"<sheets><sheet name="A" r:id="rId1"/></sheets>"#, &known);
        assert!(none.is_empty());
        assert_eq!(
            unchanged,
            r#"<sheets><sheet name="A" r:id="rId1"/></sheets>"#
        );
    }
    use calamine::{Cell, ExcelDateTime, ExcelDateTimeType};

    fn date_cell(row: u32, column: u32, serial: f64) -> Cell<Data> {
        Cell::new(
            (row, column),
            Data::DateTime(ExcelDateTime::new(
                serial,
                ExcelDateTimeType::DateTime,
                false,
            )),
        )
    }

    fn ranges(
        values: Vec<Cell<Data>>,
        formulas: Vec<Cell<String>>,
    ) -> Vec<(String, Range<Data>, Range<String>)> {
        vec![(
            "Sheet1".into(),
            Range::from_sparse(values),
            Range::from_sparse(formulas),
        )]
    }

    #[test]
    fn imports_formulas_and_compares_calculated_values_with_cached_values() {
        let imported = import_ranges(
            ranges(
                vec![
                    Cell::new((0, 0), Data::Int(2)),
                    Cell::new((1, 0), Data::Int(3)),
                    Cell::new((2, 0), Data::Int(5)),
                ],
                vec![Cell::new((2, 0), "SUM(A1:A2)".into())],
            ),
            "a".repeat(64),
            ImportLimits::default(),
        )
        .unwrap();

        assert_eq!(imported.sheets[0].name, "Sheet1");
        assert_eq!(
            imported.workbook.value(CellId::new(0, 2, 0)),
            Value::Number(5.0)
        );
        assert_eq!(
            imported.parity(),
            ParitySummary {
                formula_cells_observed: 1,
                formula_cells_loaded: 1,
                formula_cells_compared: 1,
                stored_values_matched: 1,
                stored_values_mismatched: 0,
                unsupported_formulas: 0,
            }
        );
    }

    #[test]
    fn keeps_cached_values_when_formulas_are_unsupported() {
        let imported = import_ranges(
            ranges(
                vec![Cell::new((0, 0), Data::Int(2))],
                vec![Cell::new((0, 0), "CUBEVALUE(1,2,3)".into())],
            ),
            "b".repeat(64),
            ImportLimits::default(),
        )
        .unwrap();

        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 0)),
            Value::Number(2.0)
        );
        assert_eq!(imported.unsupported.len(), 1);
        assert_eq!(imported.parity().formula_cells_compared, 0);
        assert_eq!(
            imported.unsupported[0].error,
            FormulaError::UnsupportedFunction("CUBEVALUE".into())
        );
        assert_eq!(imported.unsupported[0].kind(), "unsupported_function");
    }

    #[test]
    fn reports_bounded_unsupported_function_and_reason_distributions() {
        let imported = import_ranges(
            ranges(
                vec![Cell::new((0, 0), Data::Int(1))],
                vec![
                    Cell::new((0, 1), "TODAY()".into()),
                    Cell::new((0, 2), "today()+1".into()),
                    Cell::new((0, 3), "OFFSET(A1,1,1)".into()),
                    Cell::new((0, 4), "1+".into()),
                    Cell::new((0, 5), "Missing!A1".into()),
                    Cell::new((0, 6), "A1+1".into()),
                ],
            ),
            "i".repeat(64),
            ImportLimits::default(),
        )
        .unwrap();
        let report = imported.report();
        assert_eq!(report.schema, 2);
        assert_eq!(report.engine, ENGINE_NAME);
        assert_eq!(report.date_system, "1900");
        assert_eq!(report.formula_cells_observed, 6);
        assert_eq!(report.formula_cells_loaded, 4);
        assert_eq!(report.unsupported_formulas, 2);
        assert!(report.unsupported_functions.is_empty());
        assert_eq!(
            report.unsupported_reasons,
            BTreeMap::from([("syntax".to_string(), 1), ("unknown_sheet".to_string(), 1),])
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.starts_with("{\"schema\":2,\"engine\":\"omasheets-owned-m0\""));
        assert_eq!(serde_json::from_str::<ScoreReport>(&json).unwrap(), report);
    }

    #[test]
    fn rejects_large_used_ranges_before_materialising_cells() {
        let error = import_ranges(
            ranges(
                vec![
                    Cell::new((0, 0), Data::Int(1)),
                    Cell::new((10, 10), Data::Int(2)),
                ],
                vec![],
            ),
            "c".repeat(64),
            ImportLimits {
                max_cells: 100,
                ..ImportLimits::default()
            },
        )
        .err()
        .expect("range should be rejected");
        assert_eq!(
            error,
            ImportError::TooManyCells {
                observed: 121,
                maximum: 100,
            }
        );
    }

    #[test]
    fn preserves_absolute_coordinates_for_offset_ranges() {
        let imported = import_ranges(
            ranges(
                vec![
                    Cell::new((2, 2), Data::Int(1)),
                    Cell::new((4, 2), Data::Int(2)),
                ],
                vec![Cell::new((4, 2), "C3+1".into())],
            ),
            "d".repeat(64),
            ImportLimits::default(),
        )
        .unwrap();
        assert_eq!(
            imported.workbook.value(CellId::new(0, 4, 2)),
            Value::Number(2.0)
        );
        assert_eq!(imported.parity().stored_values_matched, 1);
    }

    #[test]
    fn rejects_formula_counts_before_loading_the_owned_graph() {
        let error = import_ranges(
            ranges(
                vec![Cell::new((0, 0), Data::Int(1))],
                vec![Cell::new((0, 0), "1+1".into())],
            ),
            "e".repeat(64),
            ImportLimits {
                max_formulas: 0,
                ..ImportLimits::default()
            },
        )
        .err()
        .expect("formula count should be rejected");
        assert_eq!(
            error,
            ImportError::TooManyFormulas {
                observed: 1,
                maximum: 0,
            }
        );
    }

    #[test]
    fn imports_date_cells_as_serials_and_matches_stored_date_formula_values() {
        let imported = import_ranges(
            ranges(
                vec![
                    date_cell(0, 0, 45_322.0), // 2024-01-31
                    Cell::new((0, 1), Data::Int(2024)),
                    Cell::new((0, 2), Data::Int(1)),
                    Cell::new((0, 3), Data::Int(31)),
                    date_cell(0, 4, 45_351.0), // EDATE clamps to 2024-02-29
                    date_cell(0, 5, 45_351.0),
                    date_cell(0, 6, 45_322.0),
                    Cell::new((0, 7), Data::Int(4)), // Wednesday
                    date_cell(1, 0, 60.0),           // the fictitious 1900-02-29
                    Cell::new((1, 1), Data::Int(1900)),
                    Cell::new((1, 2), Data::Int(2)),
                    Cell::new((1, 3), Data::Int(29)),
                ],
                vec![
                    Cell::new((0, 1), "YEAR(A1)".into()),
                    Cell::new((0, 2), "MONTH(A1)".into()),
                    Cell::new((0, 3), "DAY(A1)".into()),
                    Cell::new((0, 4), "EDATE(A1,1)".into()),
                    Cell::new((0, 5), "EOMONTH(A1,1)".into()),
                    Cell::new((0, 6), "DATE(B1,C1,D1)".into()),
                    Cell::new((0, 7), "WEEKDAY(A1)".into()),
                    Cell::new((1, 1), "YEAR(A2)".into()),
                    Cell::new((1, 2), "MONTH(A2)".into()),
                    Cell::new((1, 3), "DAY(A2)".into()),
                ],
            ),
            "g".repeat(64),
            ImportLimits::default(),
        )
        .unwrap();

        assert_eq!(imported.date_system, "1900");
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 0)),
            Value::Number(45_322.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 6)),
            Value::Number(45_322.0)
        );
        assert_eq!(
            imported.parity(),
            ParitySummary {
                formula_cells_observed: 10,
                formula_cells_loaded: 10,
                formula_cells_compared: 10,
                stored_values_matched: 10,
                stored_values_mismatched: 0,
                unsupported_formulas: 0,
            }
        );
    }

    #[test]
    fn matches_stored_text_and_boolean_formula_results() {
        let imported = import_ranges(
            ranges(
                vec![
                    Cell::new((0, 0), Data::Int(3)),
                    Cell::new((0, 1), Data::Int(9)),
                    Cell::new((0, 2), Data::String("3|9".into())),
                    Cell::new((0, 3), Data::Bool(false)),
                    Cell::new((0, 4), Data::Float(0.03)),
                    Cell::new((0, 5), Data::Bool(true)),
                ],
                vec![
                    Cell::new((0, 1), "A1^2".into()),
                    Cell::new((0, 2), "A1&\"|\"&B1".into()),
                    Cell::new((0, 3), "ISBLANK(C1)".into()),
                    Cell::new((0, 4), "A1%".into()),
                    Cell::new((0, 5), "ISTEXT(C1)".into()),
                ],
            ),
            "h".repeat(64),
            ImportLimits::default(),
        )
        .unwrap();
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 2)),
            Value::Text("3|9".into())
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 3)),
            Value::Boolean(false)
        );
        assert_eq!(imported.parity().stored_values_matched, 5);
        assert_eq!(imported.parity().stored_values_mismatched, 0);
    }

    #[test]
    fn maps_source_errors_and_defined_names_into_the_owned_engine() {
        let imported = import_ranges_with_names(
            vec![(
                "Data".into(),
                Range::from_sparse(vec![
                    Cell::new((0, 0), Data::Int(10)),
                    Cell::new((1, 0), Data::Int(20)),
                    Cell::new((0, 1), Data::Error(CellErrorType::NA)),
                    Cell::new((0, 2), Data::Int(30)),
                    Cell::new((1, 2), Data::Error(CellErrorType::NA)),
                    Cell::new((2, 2), Data::Error(CellErrorType::Ref)),
                    Cell::new((3, 2), Data::Int(2)),
                ]),
                Range::from_sparse(vec![
                    Cell::new((0, 2), "SUM(Rates)".into()),
                    Cell::new((1, 2), "B1*2".into()),
                    Cell::new((2, 2), "#REF!+1".into()),
                    Cell::new((3, 2), "Missing+1".into()),
                    Cell::new((4, 2), "[1]Other!A1".into()),
                    Cell::new((5, 2), "Broken".into()),
                ]),
            )],
            vec![
                workbook_name("Rates", "Data!$A$1:$A$2"),
                workbook_name("Broken", "[2]External!A1"),
            ],
            &[],
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
            PivotLoad::default(),
            "j".repeat(64),
            ImportLimits::default(),
        )
        .unwrap();
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 2)),
            Value::Number(30.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 1, 2)),
            Value::Error(CalcError::NotAvailable)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 2, 2)),
            Value::Error(CalcError::InvalidReference)
        );
        // No link target and no cache: the reference and the defined name
        // compile, and the missing cell is `#REF!`.
        assert_eq!(
            imported.workbook.value(CellId::new(0, 4, 2)),
            Value::Error(CalcError::InvalidReference)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 5, 2)),
            Value::Error(CalcError::InvalidReference)
        );
        let parity = imported.parity();
        assert_eq!(parity.formula_cells_loaded, 5);
        assert_eq!(parity.stored_values_matched, 3);
        assert_eq!(
            imported.report().unsupported_reasons,
            BTreeMap::from([("unknown_name".to_string(), 1)])
        );
    }

    #[test]
    fn rejects_the_1904_date_system_before_reading_any_cell() {
        assert_eq!(check_date_system(false), Ok(()));
        let error = check_date_system(true).unwrap_err();
        assert_eq!(
            error,
            ImportError::UnsupportedDateSystem { observed: "1904" }
        );
        assert_eq!(
            error.to_string(),
            "workbook uses the 1904 date system; only the 1900 date system is supported"
        );
    }

    #[test]
    fn registers_sheet_names_before_compiling_cross_sheet_formulas() {
        let imported = import_ranges(
            vec![
                (
                    "Inputs".into(),
                    Range::from_sparse(vec![Cell::new((0, 0), Data::Int(2))]),
                    Range::empty(),
                ),
                (
                    "Summary".into(),
                    Range::from_sparse(vec![Cell::new((0, 0), Data::Int(4))]),
                    Range::from_sparse(vec![Cell::new((0, 0), "Inputs!A1*2".into())]),
                ),
            ],
            "f".repeat(64),
            ImportLimits::default(),
        )
        .unwrap();
        assert_eq!(
            imported.workbook.value(CellId::new(1, 0, 0)),
            Value::Number(4.0)
        );
        assert_eq!(imported.parity().stored_values_matched, 1);
        assert!(imported.unsupported.is_empty());
    }

    fn write_owned(path: &Path, parts: &[(&str, String)]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut writer = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, body) in parts {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }

    fn content_types(extra: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>{extra}</Types>"#
        )
    }

    fn worksheet_xml(cells: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1">{cells}</row></sheetData></worksheet>"#
        )
    }

    fn xml_attr(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
    }

    fn write_plain_workbook(path: &Path, sheet: &str, cells: &str) {
        let workbook = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="{sheet}" sheetId="1" r:id="rId1"/></sheets></workbook>"#
        );
        write_owned(
            path,
            &[
                ("[Content_Types].xml", content_types("")),
                (
                    "_rels/.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_string(),
                ),
                ("xl/workbook.xml", workbook),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#.to_string(),
                ),
                ("xl/worksheets/sheet1.xml", worksheet_xml(cells)),
            ],
        );
    }

    /// `link_target` is the relationship target. `cache_value` is the
    /// external-link value for `Inputs!A1`, used only when the file is absent.
    fn write_linked_workbook(
        path: &Path,
        sheet: &str,
        cells: &str,
        defined_names: &str,
        link_target: &str,
        cache_value: &str,
    ) {
        let workbook = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="{sheet}" sheetId="1" r:id="rId1"/></sheets><externalReferences><externalReference r:id="rId2"/></externalReferences><definedNames>{defined_names}</definedNames></workbook>"#
        );
        let link = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><externalLink xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><externalBook r:id="rId1"><sheetNames><sheetName val="Inputs"/></sheetNames><sheetDataSet><sheetData sheetId="0"><row r="1"><cell r="A1"><v>{cache_value}</v></cell></row></sheetData></sheetDataSet></externalBook></externalLink>"#
        );
        let link_rels = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLinkPath" Target="{}" TargetMode="External"/></Relationships>"#,
            xml_attr(link_target)
        );
        write_owned(
            path,
            &[
                (
                    "[Content_Types].xml",
                    content_types(
                        r#"<Override PartName="/xl/externalLinks/externalLink1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.externalLink+xml"/>"#,
                    ),
                ),
                (
                    "_rels/.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_string(),
                ),
                ("xl/workbook.xml", workbook),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLink" Target="externalLinks/externalLink1.xml"/></Relationships>"#.to_string(),
                ),
                ("xl/externalLinks/externalLink1.xml", link),
                ("xl/externalLinks/_rels/externalLink1.xml.rels", link_rels),
                ("xl/worksheets/sheet1.xml", worksheet_xml(cells)),
            ],
        );
    }

    #[test]
    fn external_reference_uses_calculated_target_cell() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("omasheets-external-{}-{nonce}", std::process::id()));
        let _cleanup = TempCleanup(root.clone());
        let linked = root.join("linked");
        let book = linked.join("Book.xlsx");
        let source = linked.join("Source.xlsx");
        write_plain_workbook(&book, "Inputs", r#"<c r="A1"><f>3+7</f><v>99</v></c>"#);
        write_linked_workbook(
            &source,
            "Report",
            r#"<c r="A1"><f>[Book.xlsx]Inputs!A1+2</f><v>0</v></c><c r="B1"><f>Linked+2</f><v>0</v></c><c r="C1"><f>[1]Inputs!A1+2</f><v>0</v></c>"#,
            r#"<definedName name="Linked">[Book.xlsx]Inputs!A1</definedName>"#,
            "Book.xlsx",
            "1",
        );
        assert_eq!(
            resolve_external_path(linked.as_path(), "Book.xlsx").as_deref(),
            Some(book.as_path())
        );
        assert!(resolve_external_path(linked.as_path(), "https://example.com/Book.xlsx").is_none());
        assert!(resolve_external_path(linked.as_path(), "../Book.xlsx").is_none());

        let imported = import_xlsx(&source, ImportLimits::default()).unwrap();
        assert!(imported.unsupported.is_empty());
        // The link cache says 1. The file's formula calculates to 10, so the
        // source formula is 12, not 3.
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 0)),
            Value::Number(12.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 1)),
            Value::Number(12.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 2)),
            Value::Number(12.0)
        );

        let absent_dir = root.join("absent");
        let absent = absent_dir.join("Absent.xlsx");
        write_linked_workbook(
            &absent,
            "Report",
            r#"<c r="A1"><f>[Missing.xlsx]Inputs!A1+2</f><v>0</v></c><c r="B1"><f>Linked+2</f><v>0</v></c>"#,
            r#"<definedName name="Linked">[Missing.xlsx]Inputs!A1</definedName>"#,
            "Missing.xlsx",
            "10",
        );
        let absent_imported = import_xlsx(&absent, ImportLimits::default()).unwrap();
        assert!(absent_imported.unsupported.is_empty());
        assert_eq!(
            absent_imported.workbook.value(CellId::new(0, 0, 0)),
            Value::Number(12.0)
        );
        assert_eq!(
            absent_imported.workbook.value(CellId::new(0, 0, 1)),
            Value::Number(12.0)
        );

        let outside = root.join("outside");
        let secret = outside.join("Secret.xlsx");
        write_plain_workbook(&secret, "Inputs", r#"<c r="A1"><f>3+7</f><v>99</v></c>"#);
        let absolute = secret.canonicalize().unwrap();
        let absolute_dir = root.join("absolute");
        let absolute_source = absolute_dir.join("Source.xlsx");
        write_linked_workbook(
            &absolute_source,
            "Report",
            r#"<c r="A1"><f>[Secret.xlsx]Inputs!A1+2</f><v>0</v></c>"#,
            "",
            absolute.to_str().unwrap(),
            "4",
        );
        assert!(
            resolve_external_path(absolute_dir.as_path(), absolute.to_str().unwrap()).is_none()
        );
        assert!(
            resolve_external_path(
                absolute_dir.as_path(),
                &format!("file://{}", absolute.display())
            )
            .is_none()
        );
        let absolute_imported = import_xlsx(&absolute_source, ImportLimits::default()).unwrap();
        assert_eq!(
            absolute_imported.workbook.value(CellId::new(0, 0, 0)),
            Value::Number(6.0)
        );

        let cycle_dir = root.join("cycle");
        let cycle = cycle_dir.join("Self.xlsx");
        write_linked_workbook(
            &cycle,
            "Report",
            r#"<c r="A1"><f>[Self.xlsx]Inputs!A1+2</f><v>0</v></c>"#,
            "",
            "Self.xlsx",
            "10",
        );
        let cycle_imported = import_xlsx(&cycle, ImportLimits::default()).unwrap();
        assert_eq!(
            cycle_imported.workbook.value(CellId::new(0, 0, 0)),
            Value::Number(12.0)
        );
    }

    struct TempCleanup(std::path::PathBuf);

    impl Drop for TempCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Empty self-closing `sheetData` elements, a cached `E10` of 33926, a
    /// blank external cell that must show 0, and `TODAY()`/`NOW()` replayed
    /// from their cached serials. A same-named file for an absolute target
    /// must not replace that cache.
    #[test]
    fn enron_mismatch_imports_cached_today_and_external_cell() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "omasheets-enron-mismatch-{}-{nonce}",
            std::process::id()
        ));
        let _cleanup = TempCleanup(root.clone());
        let source_dir = root.join("source");
        let source = source_dir.join("Source.xlsx");
        let cells = r#"<row r="1"><c r="A1"><f>TODAY()</f><v>41885</v></c><c r="B1"><f>A1+1</f><v>41886</v></c><c r="C1"><f>NOW()</f><v>41885.25</v></c><c r="D1"><f>[1]Missing!A1</f><v>0</v></c></row><row r="10"><c r="E10"><f>[1]Nominations!E$10</f><v>33926</v></c><c r="Z10"><f>[1]Nominations!Z$10</f><v>0</v></c></row>"#;
        let link = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><externalLink xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><externalBook r:id="rId1"><sheetNames><sheetName val="Summary"/><sheetName val="FUGG OBA"/><sheetName val="Nominations"/></sheetNames><sheetDataSet><sheetData sheetId="0"/><sheetData sheetId="1"/><sheetData sheetId="2"><row r="10"><cell r="E10"><v>33926</v></cell></row></sheetData></sheetDataSet></externalBook></externalLink>"#;
        write_custom_linked_workbook(
            &source,
            "Nominations",
            &format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{cells}</sheetData></worksheet>"#
            ),
            "",
            "Nov%20OBA%20Balance.xls",
            link,
        );
        let imported = import_xlsx(&source, ImportLimits::default()).unwrap();
        assert!(imported.unsupported.is_empty());
        assert_eq!(
            imported.workbook.value(CellId::new(0, 9, 4)),
            Value::Number(33926.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 9, 25)),
            Value::Number(0.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 0)),
            Value::Number(41885.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 1)),
            Value::Number(41886.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 2)),
            Value::Number(41885.25)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 3)),
            Value::Error(CalcError::InvalidReference)
        );

        let decoy_dir = root.join("decoy");
        let decoy = decoy_dir.join("Nominations.xlsx");
        write_plain_workbook(&decoy, "Nominations", r#"<c r="E10"><v>7</v></c>"#);
        let flat = root.join("flat");
        let flat_decoy = flat.join("Nominations.xlsx");
        write_plain_workbook(&flat_decoy, "Nominations", r#"<c r="E10"><v>7</v></c>"#);
        let absolute = decoy.canonicalize().unwrap();
        let book = flat.join("Book.xlsx");
        write_custom_linked_workbook(
            &book,
            "Report",
            &worksheet_xml(r#"<c r="E10"><f>[1]Nominations!E$10</f><v>33926</v></c>"#),
            "",
            absolute.to_str().unwrap(),
            link,
        );
        let kept = import_xlsx(&book, ImportLimits::default()).unwrap();
        assert_eq!(
            kept.workbook.value(CellId::new(0, 9, 4)),
            Value::Number(33926.0),
            "a same-named file must not replace the populated cache"
        );
    }

    fn write_custom_linked_workbook(
        path: &Path,
        sheet: &str,
        worksheet: &str,
        defined_names: &str,
        link_target: &str,
        link: &str,
    ) {
        let workbook = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="{sheet}" sheetId="1" r:id="rId1"/></sheets><externalReferences><externalReference r:id="rId2"/></externalReferences><definedNames>{defined_names}</definedNames></workbook>"#
        );
        let link_rels = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLinkPath" Target="{}" TargetMode="External"/></Relationships>"#,
            xml_attr(link_target)
        );
        write_owned(
            path,
            &[
                (
                    "[Content_Types].xml",
                    content_types(
                        r#"<Override PartName="/xl/externalLinks/externalLink1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.externalLink+xml"/>"#,
                    ),
                ),
                (
                    "_rels/.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_string(),
                ),
                ("xl/workbook.xml", workbook),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLink" Target="externalLinks/externalLink1.xml"/></Relationships>"#.to_string(),
                ),
                ("xl/externalLinks/externalLink1.xml", link.to_string()),
                ("xl/externalLinks/_rels/externalLink1.xml.rels", link_rels),
                ("xl/worksheets/sheet1.xml", worksheet.to_string()),
            ],
        );
    }

    #[test]
    fn this_row_reads_the_table_loaded_from_the_package() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("omasheets-table-{}-{nonce}", std::process::id()));
        let _cleanup = TempCleanup(root.clone());
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("Table.xlsx");
        let table = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><table xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" id="1" name="Lines" displayName="Lines" ref="A1:B3" totalsRowCount="1"><autoFilter ref="A1:B2"/><tableColumns count="2"><tableColumn id="1" name="Amount"/><tableColumn id="2" name="Twice"/></tableColumns></table>"#;
        write_owned(
            &source,
            &[
                (
                    "[Content_Types].xml",
                    content_types(
                        r#"<Override PartName="/xl/tables/table1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml"/>"#,
                    ),
                ),
                (
                    "_rels/.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_string(),
                ),
                (
                    "xl/workbook.xml",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#.to_string(),
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#.to_string(),
                ),
                (
                    "xl/worksheets/_rels/sheet1.xml.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table1.xml"/></Relationships>"#.to_string(),
                ),
                ("xl/tables/table1.xml", table.to_string()),
                (
                    "xl/worksheets/sheet1.xml",
                    worksheet_xml(
                        r#"<c r="A2"><v>2</v></c><c r="B2"><f>Lines[[#This Row],[Amount]]*2</f><v>0</v></c><c r="A3"><f>SUM(Lines[Amount])</f><v>0</v></c>"#,
                    ),
                ),
            ],
        );
        let imported = import_xlsx(&source, ImportLimits::default()).unwrap();
        assert!(
            imported.unsupported.is_empty(),
            "{:?}",
            imported.unsupported
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 1, 1)),
            Value::Number(4.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 2, 0)),
            Value::Number(2.0)
        );
    }

    #[test]
    fn yearfrac_of_today_replays_from_the_stored_result() {
        let mut workbook = Workbook::default();
        workbook.set_number(CellId::new(0, 5, 0), 36982.0);
        workbook.set_number(CellId::new(0, 5, 8), 0.05);
        let at = omasheets_calc::serial_date::unix_millis_from_serial(41885.0).unwrap();
        workbook.set_tick(at);
        workbook
            .set_formula(CellId::new(0, 5, 9), "1/(1+I6)^YEARFRAC(TODAY(),A6,1)")
            .unwrap();
        let Value::Number(expected) = workbook.value(CellId::new(0, 5, 9)) else {
            panic!("yearfrac did not return a number");
        };
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("omasheets-yearfrac-{}-{nonce}", std::process::id()));
        let _cleanup = TempCleanup(root.clone());
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("Year.xlsx");
        write_plain_workbook(
            &source,
            "S",
            &format!(
                r#"<c r="A6"><v>36982</v></c><c r="I6"><v>0.05</v></c><c r="J6"><f>1/(1+I6)^YEARFRAC(TODAY(),A6,1)</f><v>{expected}</v></c>"#
            ),
        );
        let imported = import_xlsx(&source, ImportLimits::default()).unwrap();
        assert_eq!(
            imported.workbook.value(CellId::new(0, 5, 9)),
            Value::Number(expected)
        );
    }

    #[test]
    fn external_cache_keeps_text_spaces_and_refuses_a_failed_refresh() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "omasheets-external-space-{}-{nonce}",
            std::process::id()
        ));
        let _cleanup = TempCleanup(root.clone());
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("Source.xlsx");
        let link = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><externalLink xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><externalBook r:id="rId1"><sheetNames><sheetName val="Headers"/><sheetName val="Broken"/></sheetNames><sheetDataSet><sheetData sheetId="0"><row r="1"><cell r="A1" t="str"><v>other</v></cell><cell r="B1" t="str"><v xml:space="preserve">   CGPR-STN2   </v></cell><cell r="C1" t="str"><v/></cell></row></sheetData><sheetData sheetId="1" refreshError="1"><row r="1"><cell r="A1"><v>9</v></cell></row></sheetData></sheetDataSet></externalBook></externalLink>"#;
        write_owned(
            &source,
            &[
                (
                    "[Content_Types].xml",
                    content_types(
                        r#"<Override PartName="/xl/externalLinks/externalLink1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.externalLink+xml"/>"#,
                    ),
                ),
                (
                    "_rels/.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_string(),
                ),
                (
                    "xl/workbook.xml",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Report" sheetId="1" r:id="rId1"/></sheets><externalReferences><externalReference r:id="rId2"/></externalReferences></workbook>"#.to_string(),
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLink" Target="externalLinks/externalLink1.xml"/></Relationships>"#.to_string(),
                ),
                ("xl/externalLinks/externalLink1.xml", link.to_string()),
                (
                    "xl/externalLinks/_rels/externalLink1.xml.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLinkPath" Target="missing.xls" TargetMode="External"/></Relationships>"#.to_string(),
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    worksheet_xml(
                        r#"<c r="A1" t="inlineStr"><is><t xml:space="preserve">   CGPR-STN2   </t></is></c><c r="B1"><f>MATCH(A1,[1]Headers!$A$1:$B$1,0)</f><v>0</v></c><c r="C1"><f>[1]Broken!A1</f><v>9</v></c><c r="D1"><f>[1]Broken!B1</f><v>0</v></c><c r="E1"><f>[1]Headers!C1</f><v></v></c>"#,
                    ),
                ),
            ],
        );
        let imported = import_xlsx(&source, ImportLimits::default()).unwrap();
        assert!(
            imported.unsupported.is_empty(),
            "{:?}",
            imported.unsupported
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 1)),
            Value::Number(2.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 2)),
            Value::Number(9.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 3)),
            Value::Error(CalcError::InvalidReference)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 0, 4)),
            Value::Text(String::new())
        );
    }

    #[test]
    fn enron_mismatch_real_enron_files_replay_33926_and_today_serial() {
        let root = Path::new("/Users/markwatts/omasheets-corpus/enron-figshare/enron-figshare");
        let external = root.join("paul_lucci__28399__Enron Daily Email Nov '01.xlsx");
        let today = root.join("kevin_presto__19778__Crude.xlsx");
        if !external.is_file() || !today.is_file() {
            return;
        }
        let imported = import_xlsx(&external, ImportLimits::default()).unwrap();
        assert_eq!(
            imported.workbook.value(CellId::new(0, 9, 4)),
            Value::Number(33926.0)
        );
        assert_eq!(
            imported.workbook.value(CellId::new(0, 9, 25)),
            Value::Number(0.0)
        );
        assert_eq!(imported.parity().stored_values_mismatched, 0);
        let crude = import_xlsx(&today, ImportLimits::default()).unwrap();
        let main = crude
            .sheets
            .iter()
            .find(|sheet| sheet.name == "MAIN")
            .expect("MAIN");
        assert_eq!(
            crude.workbook.value(CellId::new(main.index, 30, 6)),
            Value::Number(41885.0)
        );
    }

    #[test]
    fn enron_ref_cluster_reads_failed_refresh_cache_and_built_indirect() {
        let root = Path::new("/Users/markwatts/omasheets-corpus/enron-figshare/enron-figshare");
        let pnl = root.join("stacey_white__38996__Pwr west P&L.xlsx");
        let radar = root.join("jeffrey_a_shankman__13936__RADAR Screens-4Q 1116.xlsx");
        let parks = root.join("joe_parks__14513__P&L_JUN01.xlsx");
        if !pnl.is_file() || !radar.is_file() || !parks.is_file() {
            return;
        }
        assert_eq!(
            import_xlsx(&pnl, ImportLimits::default())
                .unwrap()
                .parity()
                .stored_values_mismatched,
            0
        );
        let radar = import_xlsx(&radar, ImportLimits::default()).unwrap();
        assert_eq!(
            radar.workbook.value(CellId::new(0, 15, 11)),
            Value::Number(32.5)
        );
        // CELL("filename") now calculates. The cached text is the Windows path
        // from the machine that last saved the file, so these two cells stay
        // mismatches. Every other formula on the sheet matches.
        let filename_mismatches = radar
            .mismatched_cells()
            .filter(|(_, _, calculated)| calculated == &Value::Text(String::new()))
            .count();
        assert_eq!(radar.parity().stored_values_mismatched, filename_mismatches);
        assert_eq!(filename_mismatches, 2);
        assert_eq!(
            import_xlsx(&parks, ImportLimits::default())
                .unwrap()
                .parity()
                .stored_values_mismatched,
            0
        );
    }

    #[test]
    fn operating_model_offset_reads_the_shifted_row() {
        let path = Path::new(
            "/Users/markwatts/omasheets-corpus/spreadsheet-rl-2026/sample/spreadsheetbench_2__Debugging__08_07__input.xlsx",
        );
        if !path.is_file() {
            return;
        }
        let imported = import_xlsx(path, ImportLimits::default()).unwrap();
        assert_eq!(imported.parity().stored_values_mismatched, 0);
        assert_eq!(
            imported
                .unsupported_reasons()
                .get("cycle")
                .copied()
                .unwrap_or(0),
            0
        );
    }

    fn assert_close_number(value: Value, expected: f64) {
        match value {
            Value::Number(number) => {
                assert!((number - expected).abs() <= 1e-4, "{number} != {expected}")
            }
            other => panic!("expected {expected}, got {other:?}"),
        }
    }

    #[test]
    fn getpivotdata_imports_a_date_filter_and_field_items() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "omasheets-getpivotdata-{}-{nonce}",
            std::process::id()
        ));
        let _cleanup = TempCleanup(root.clone());
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("Pivot.xlsx");
        let sheet = r#"<c r="A1"><v>1</v></c><c r="B1"><f>GETPIVOTDATA("Amount",$A$1)</f><v>20</v></c><c r="C1"><f>GETPIVOTDATA("Amount",$A$1,"Region","East")</f><v>20</v></c><c r="D1"><f>GETPIVOTDATA("Amount",$A$1,"Region","North")</f></c><c r="E1"><f>GETPIVOTDATA("Amount",$A$1,"When",44927)</f><v>7</v></c><c r="F1"><f>GETPIVOTDATA("Amount",$A$1,"Region")</f></c><c r="G1"><f>GETPIVOTDATA("Amount",$Z$1)</f></c><c r="H1"><f>GETPIVOTDATA("Bonus",$A$1)</f><v>0</v></c><c r="I1"><f>GETPIVOTDATA("Rows",$A$1)</f></c><c r="J1"><f>_xlfn.GETPIVOTDATA("Amount",$A$1,"Region","East")</f><v>20</v></c><c r="K1"><f>GETPIVOTDATA("Amount",$A$1,"Region","West")</f></c>"#;
        let pivot = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><pivotTableDefinition xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" name="P" cacheId="1"><location ref="A1" firstHeaderRow="0" firstDataRow="0" firstDataCol="0"/><pivotFields count="4"><pivotField axis="axisRow"><items count="2"><item x="0"/><item x="1"/></items></pivotField><pivotField dataField="1"/><pivotField axis="axisRow"><items count="1"><item x="0"/></items></pivotField><pivotField dataField="1"/></pivotFields><rowFields count="2"><field x="0"/><field x="2"/></rowFields><dataFields count="3"><dataField name="Amount" fld="1"/><dataField name="Bonus" fld="3"/><dataField name="Rows" fld="0" subtotal="count"/></dataFields><filters count="1"><filter fld="2" type="dateBetween" id="1" evalOrder="0"><autoFilter><filterColumn colId="0"><customFilters and="1"><customFilter operator="greaterThanOrEqual" val="44927"/><customFilter operator="lessThanOrEqual" val="44957"/></customFilters></filterColumn></autoFilter></filter></filters></pivotTableDefinition>"#;
        let definition = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><pivotCacheDefinition xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="rId1" recordCount="6"><cacheSource type="worksheet"><worksheetSource ref="A1:D6" sheet="Data"/></cacheSource><cacheFields count="4"><cacheField name="Region"><sharedItems count="2"><s v="East"/><s v="West"/></sharedItems></cacheField><cacheField name="Amount"><sharedItems containsNumber="1"/></cacheField><cacheField name="When"><sharedItems containsDate="1"/></cacheField><cacheField name="Bonus"><sharedItems containsBlank="1" containsNumber="1"/></cacheField></cacheFields></pivotCacheDefinition>"#;
        let records = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><pivotCacheRecords xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="6"><r><x v="0"/><n v="10"/><d v="2023-01-15T00:00:00"/><m/></r><r><x v="1"/><n v="25"/><d v="2023-02-15T00:00:00"/><n v="5"/></r><r><x v="0"/><m/><d v="2023-01-20T00:00:00"/><m/></r><r><x v="0"/><n v="7"/><d v="2023-01-01T00:00:00"/><n v="0"/></r><r><x v="0"/><n v="3"/><d v="2023-01-31T00:00:00"/><m/></r><r><x v="0"/><n v="100"/><d v="2023-02-01T00:00:00"/><m/></r></pivotCacheRecords>"#;
        write_owned(
            &source,
            &[
                ("[Content_Types].xml", content_types("")),
                (
                    "_rels/.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_string(),
                ),
                (
                    "xl/workbook.xml",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#.to_string(),
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#.to_string(),
                ),
                ("xl/worksheets/sheet1.xml", worksheet_xml(sheet)),
                (
                    "xl/worksheets/_rels/sheet1.xml.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/pivotTable" Target="../pivotTables/pivotTable1.xml"/></Relationships>"#.to_string(),
                ),
                ("xl/pivotTables/pivotTable1.xml", pivot.to_string()),
                (
                    "xl/pivotTables/_rels/pivotTable1.xml.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/pivotCacheDefinition" Target="../pivotCache/pivotCacheDefinition1.xml"/></Relationships>"#.to_string(),
                ),
                ("xl/pivotCache/pivotCacheDefinition1.xml", definition.to_string()),
                (
                    "xl/pivotCache/_rels/pivotCacheDefinition1.xml.rels",
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/pivotCacheRecords" Target="pivotCacheRecords1.xml"/></Relationships>"#.to_string(),
                ),
                ("xl/pivotCache/pivotCacheRecords1.xml", records.to_string()),
            ],
        );
        let imported = import_xlsx(&source, ImportLimits::default()).unwrap();
        let ref_error = Value::Error(CalcError::InvalidReference);
        let cases = [
            (1, Value::Number(20.0)),
            (2, Value::Number(20.0)),
            (3, ref_error.clone()),
            (4, Value::Number(7.0)),
            (5, ref_error.clone()),
            (6, ref_error.clone()),
            (7, Value::Number(0.0)),
            (8, Value::Error(CalcError::InvalidValue)),
            (9, Value::Number(20.0)),
            (10, ref_error),
        ];
        for (column, expected) in cases {
            let cell = CellId::new(0, 0, column);
            assert!(
                imported.unsupported.iter().all(|item| item.cell != cell),
                "{:?}",
                imported.unsupported
            );
            assert_eq!(imported.workbook.value(cell), expected, "column {column}");
        }
    }

    #[test]
    fn getpivotdata_sample_matches_stored_totals() {
        let path = Path::new(
            "/Users/markwatts/omasheets-corpus/spreadsheet-rl-2026/sample/excelforum__excel-formulas-and-functions__task-job-47b8ffb242e1996d__input.xlsx",
        );
        if !path.is_file() {
            return;
        }
        let imported = import_xlsx(path, ImportLimits::default()).unwrap();
        let sheet = imported
            .sheets
            .iter()
            .find(|sheet| sheet.name == "PTHeadline")
            .expect("PTHeadline");
        let expected = [3_321_428.62, -62_132.69, 119_081.74, 0.0];
        for (offset, expected) in expected.into_iter().enumerate() {
            let cell = CellId::new(sheet.index, 5 + offset as u32, 10);
            assert!(
                imported.unsupported.iter().all(|item| item.cell != cell),
                "{cell:?} {:?}",
                imported.unsupported.iter().find(|item| item.cell == cell)
            );
            assert_close_number(imported.workbook.value(cell), expected);
        }
    }

    #[test]
    fn datevalue_external_text_linest_and_yearfrac_match_stored_values() {
        let root = Path::new("/Users/markwatts/omasheets-corpus/enron-figshare/enron-figshare");
        let files = [
            "andy_zipper__123__Broker & Exchange Detail 6-7-01.xlsx",
            "cooper_richey__4122__enron.xlsx",
            "john_griffith__15855__Vol Move.xlsx",
            "frank_ermis__11027__AEC Volumes 021301.xlsx",
        ];
        if files.iter().any(|name| !root.join(name).is_file()) {
            return;
        }
        for name in files {
            let imported = import_xlsx(&root.join(name), ImportLimits::default()).unwrap();
            let misses: Vec<_> = imported
                .mismatched_cells()
                .map(|(cell, stored, calculated)| format!("{cell:?} {stored:?} {calculated:?}"))
                .take(3)
                .collect();
            assert_eq!(
                imported.parity().stored_values_mismatched,
                0,
                "{name} {misses:?}"
            );
        }
    }

    #[test]
    fn dcf_model_offset_drivers_match_stored_values() {
        let path = Path::new("/Users/markwatts/omasheets-corpus/spreadsheet-rl-2026/sample/spreadsheetbench_2__Financial_Model__08_04__input.xlsx");
        if !path.is_file() {
            return;
        }
        let imported = import_xlsx(path, ImportLimits::default()).unwrap();
        let report = imported.report();
        // The opening balance matches until the schedule switches to the
        // constant payment. PMT is high by about 1.29e-8, and that excess
        // compounds to about 1.3e-6 by the last draw. The seven cycles are an
        // annual total that sums the months which are fractions of that total.
        assert_eq!(
            (
                report.stored_values_mismatched,
                report.unsupported_reasons.get("cycle").copied()
            ),
            (5, Some(7)),
            "{:?}",
            report.mismatch_value_kinds
        );
    }
}
