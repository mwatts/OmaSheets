//! Visible-tile appearance read from an `.xlsx` package.
//!
//! Only explicit `rgb` attributes are kept. Theme and indexed colors are
//! ignored. No GPUI [`Window`](gpui_kit::Window) or [`App`](gpui_kit::App)
//! is touched.

use omasheets_core::parse_a1;
use std::fs::File;
use std::io::Read;
use std::path::Path;

const PART_LIMIT: u64 = 8 * 1024 * 1024;
const EXCEL_DEFAULT_WIDTH: f64 = 8.43;

/// Zero-based inclusive window the grid is about to paint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibleWindow {
    pub origin_row: u32,
    pub origin_column: u32,
    pub rows: u32,
    pub columns: u32,
}

/// Explicit sRGB color from an `rgb` attribute. Alpha in the package is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RgbColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl RgbColor {
    pub fn packed(self) -> u32 {
        (u32::from(self.red) << 16) | (u32::from(self.green) << 8) | u32::from(self.blue)
    }
}

/// One column's width in Excel character units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnWidth {
    pub column: u32,
    pub width: f64,
    /// `customWidth` is set, or the stored width is not Excel's default.
    pub custom: bool,
}

/// Inclusive merge rectangle in zero-based row and column indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeRect {
    pub start_row: u32,
    pub start_column: u32,
    pub end_row: u32,
    pub end_column: u32,
}

impl MergeRect {
    pub fn covers(self, row: u32, column: u32) -> bool {
        (self.start_row..=self.end_row).contains(&row)
            && (self.start_column..=self.end_column).contains(&column)
    }

    pub fn intersects(self, window: VisibleWindow) -> bool {
        let Some(last_row) = window_last(window.origin_row, window.rows) else {
            return false;
        };
        let Some(last_column) = window_last(window.origin_column, window.columns) else {
            return false;
        };
        self.start_row <= last_row
            && window.origin_row <= self.end_row
            && self.start_column <= last_column
            && window.origin_column <= self.end_column
    }
}

/// A cell inside the tile that carries an explicit fill or font color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellColor {
    pub row: u32,
    pub column: u32,
    pub fill: Option<RgbColor>,
    pub font: Option<RgbColor>,
}

/// Facts the grid paints for one visible tile of the first worksheet.
#[derive(Clone, Debug, PartialEq)]
pub struct AppearanceTile {
    pub sheet_name: String,
    pub column_widths: Vec<ColumnWidth>,
    pub merges: Vec<MergeRect>,
    pub colors: Vec<CellColor>,
}

impl AppearanceTile {
    pub fn fill(&self, row: u32, column: u32) -> Option<RgbColor> {
        self.colors
            .iter()
            .find(|cell| cell.row == row && cell.column == column)
            .and_then(|cell| cell.fill)
    }

    pub fn font(&self, row: u32, column: u32) -> Option<RgbColor> {
        self.colors
            .iter()
            .find(|cell| cell.row == row && cell.column == column)
            .and_then(|cell| cell.font)
    }

    pub fn width(&self, column: u32) -> Option<f64> {
        self.column_widths
            .iter()
            .find(|entry| entry.column == column)
            .map(|entry| entry.width)
    }

    pub fn merge_at(&self, row: u32, column: u32) -> Option<MergeRect> {
        self.merges
            .iter()
            .copied()
            .find(|merge| merge.covers(row, column))
    }
}

#[derive(Debug)]
pub enum AppearanceError {
    Io(std::io::Error),
    Zip(String),
    Package(String),
}

impl std::fmt::Display for AppearanceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Zip(error) | Self::Package(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for AppearanceError {}

impl From<std::io::Error> for AppearanceError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Reads one xlsx and returns column widths, merges, and explicit RGB colors
/// that intersect `window`. Theme and indexed colors are dropped.
pub fn project_xlsx_appearance(
    path: &Path,
    window: VisibleWindow,
) -> Result<AppearanceTile, AppearanceError> {
    let file = File::open(path)?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| AppearanceError::Zip(error.to_string()))?;
    let workbook = read_part(&mut archive, "xl/workbook.xml")?;
    let relationships = read_part(&mut archive, "xl/_rels/workbook.xml.rels")?;
    let (sheet_name, sheet_part) = first_sheet(&workbook, &relationships)?;
    let sheet = read_part(&mut archive, &sheet_part)?;
    let styles = read_part(&mut archive, "xl/styles.xml").unwrap_or_default();
    let palette = StylePalette::parse(&styles);
    Ok(project_sheet(&sheet_name, &sheet, &palette, window))
}

fn project_sheet(
    sheet_name: &str,
    sheet: &str,
    palette: &StylePalette,
    window: VisibleWindow,
) -> AppearanceTile {
    let mut column_widths = Vec::new();
    for tag in start_tags(sheet, "col") {
        let Some(min) = attribute(&tag, "min").and_then(|text| text.parse::<u32>().ok()) else {
            continue;
        };
        let max = attribute(&tag, "max")
            .and_then(|text| text.parse::<u32>().ok())
            .unwrap_or(min);
        let Some(width) = attribute(&tag, "width").and_then(|text| text.parse::<f64>().ok()) else {
            continue;
        };
        if !width.is_finite() {
            continue;
        }
        let flagged = attribute(&tag, "customWidth").is_some_and(|value| is_truthy(&value));
        let custom = flagged || (width - EXCEL_DEFAULT_WIDTH).abs() > 0.05;
        let Some(last) = window_last(window.origin_column, window.columns) else {
            continue;
        };
        let start = min.max(1);
        let end = max.max(start);
        for column in start..=end {
            let index = column - 1;
            if index < window.origin_column || index > last {
                continue;
            }
            column_widths.push(ColumnWidth {
                column: index,
                width,
                custom,
            });
            if column_widths.len() > 512 {
                break;
            }
        }
    }

    let mut merges = Vec::new();
    for tag in start_tags(sheet, "mergeCell") {
        let Some(reference) = attribute(&tag, "ref") else {
            continue;
        };
        let Some(merge) = parse_merge(&reference) else {
            continue;
        };
        if merge.intersects(window) {
            merges.push(merge);
        }
        if merges.len() > 4_096 {
            break;
        }
    }

    let mut colors = Vec::new();
    for tag in start_tags(sheet, "c") {
        let Some(reference) = attribute(&tag, "r") else {
            continue;
        };
        let Some(style) = attribute(&tag, "s").and_then(|text| text.parse::<usize>().ok()) else {
            continue;
        };
        let Some((row, column)) = parse_a1(&reference) else {
            continue;
        };
        let row = row as u32;
        let column = column as u32;
        if !in_window(window, row, column) {
            continue;
        }
        let (font, fill) = palette.resolve(style);
        if font.is_none() && fill.is_none() {
            continue;
        }
        colors.push(CellColor {
            row,
            column,
            fill,
            font,
        });
        if colors.len() > 8_192 {
            break;
        }
    }

    AppearanceTile {
        sheet_name: sheet_name.to_string(),
        column_widths,
        merges,
        colors,
    }
}

struct StylePalette {
    fonts: Vec<Option<RgbColor>>,
    fills: Vec<Option<RgbColor>>,
    xfs: Vec<(usize, usize)>,
}

impl StylePalette {
    fn parse(styles: &str) -> Self {
        let fonts = element_bodies(section(styles, "fonts"), "font")
            .into_iter()
            .map(|body| rgb_attribute(&body))
            .collect();
        let fills = element_bodies(section(styles, "fills"), "fill")
            .into_iter()
            .map(|body| rgb_attribute(&body))
            .collect();
        let xfs = start_tags(section(styles, "cellXfs"), "xf")
            .into_iter()
            .map(|tag| {
                let font = attribute(&tag, "fontId")
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(0);
                let fill = attribute(&tag, "fillId")
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(0);
                (font, fill)
            })
            .collect();
        Self { fonts, fills, xfs }
    }

    fn resolve(&self, style: usize) -> (Option<RgbColor>, Option<RgbColor>) {
        let Some(&(font, fill)) = self.xfs.get(style) else {
            return (None, None);
        };
        (
            self.fonts.get(font).copied().flatten(),
            self.fills.get(fill).copied().flatten(),
        )
    }
}

fn first_sheet(workbook: &str, relationships: &str) -> Result<(String, String), AppearanceError> {
    let sheet = start_tags(workbook, "sheet")
        .into_iter()
        .next()
        .ok_or_else(|| AppearanceError::Package("workbook has no sheet".into()))?;
    let name = attribute(&sheet, "name")
        .ok_or_else(|| AppearanceError::Package("sheet has no name".into()))?;
    let id = attribute(&sheet, "r:id")
        .or_else(|| attribute_suffix(&sheet, "id"))
        .ok_or_else(|| AppearanceError::Package("sheet has no relationship id".into()))?;
    let relationship = start_tags(relationships, "Relationship")
        .into_iter()
        .find(|tag| attribute(tag, "Id").as_deref() == Some(id.as_str()))
        .ok_or_else(|| AppearanceError::Package(format!("missing relationship {id}")))?;
    let target = attribute(&relationship, "Target")
        .ok_or_else(|| AppearanceError::Package(format!("relationship {id} has no target")))?;
    Ok((unescape(&name), worksheet_part(&target)))
}

fn worksheet_part(target: &str) -> String {
    let target = target.trim_start_matches('/');
    if target.starts_with("xl/") {
        target.to_string()
    } else {
        format!("xl/{target}")
    }
}

fn read_part(archive: &mut zip::ZipArchive<File>, name: &str) -> Result<String, AppearanceError> {
    let mut part = archive
        .by_name(name)
        .map_err(|error| AppearanceError::Zip(format!("{name}: {error}")))?;
    if part.size() > PART_LIMIT {
        return Err(AppearanceError::Package(format!(
            "{name} exceeds the part size limit"
        )));
    }
    let mut text = String::new();
    part.read_to_string(&mut text)?;
    Ok(text)
}

fn parse_merge(reference: &str) -> Option<MergeRect> {
    let (start, end) = reference.split_once(':').unwrap_or((reference, reference));
    let (start_row, start_column) = parse_a1(start)?;
    let (end_row, end_column) = parse_a1(end)?;
    Some(MergeRect {
        start_row: start_row.min(end_row) as u32,
        start_column: start_column.min(end_column) as u32,
        end_row: start_row.max(end_row) as u32,
        end_column: start_column.max(end_column) as u32,
    })
}

fn window_last(origin: u32, len: u32) -> Option<u32> {
    if len == 0 {
        None
    } else {
        Some(origin.saturating_add(len - 1))
    }
}

fn in_window(window: VisibleWindow, row: u32, column: u32) -> bool {
    let Some(last_row) = window_last(window.origin_row, window.rows) else {
        return false;
    };
    let Some(last_column) = window_last(window.origin_column, window.columns) else {
        return false;
    };
    (window.origin_row..=last_row).contains(&row)
        && (window.origin_column..=last_column).contains(&column)
}

fn is_truthy(value: &str) -> bool {
    matches!(value, "1" | "true" | "TRUE")
}

fn rgb_attribute(body: &str) -> Option<RgbColor> {
    let raw = attribute(body, "rgb")?;
    parse_rgb(&raw)
}

fn parse_rgb(text: &str) -> Option<RgbColor> {
    let hex = text.trim();
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let (red, green, blue) = match hex.len() {
        8 => (
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
            u8::from_str_radix(&hex[6..8], 16).ok()?,
        ),
        6 => (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        ),
        _ => return None,
    };
    Some(RgbColor { red, green, blue })
}

/// Body of the first element named `name`.
fn section<'a>(xml: &'a str, name: &str) -> &'a str {
    let close = format!("</{name}>");
    let mut index = 0;
    while let Some(relative) = xml[index..].find('<') {
        let at = index + relative;
        if is_open_tag(xml, at, name) {
            let Some(tag_end) = xml[at..].find('>') else {
                return "";
            };
            let header = &xml[at..at + tag_end + 1];
            if header.ends_with("/>") {
                return "";
            }
            let body_at = at + tag_end + 1;
            let Some(close_at) = xml[body_at..].find(&close) else {
                return "";
            };
            return &xml[body_at..body_at + close_at];
        }
        index = at + 1;
    }
    ""
}

fn start_tags(xml: &str, name: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut index = 0;
    while let Some(relative) = xml[index..].find('<') {
        let at = index + relative;
        if is_open_tag(xml, at, name) {
            if let Some(end) = xml[at..].find('>') {
                tags.push(xml[at..at + end + 1].to_string());
                index = at + end + 1;
                continue;
            }
        }
        index = at + 1;
    }
    tags
}

fn element_bodies(xml: &str, name: &str) -> Vec<String> {
    let mut bodies = Vec::new();
    let mut index = 0;
    let close = format!("</{name}>");
    while let Some(relative) = xml[index..].find('<') {
        let at = index + relative;
        if is_open_tag(xml, at, name) {
            let Some(tag_end) = xml[at..].find('>') else {
                break;
            };
            let header = &xml[at..at + tag_end + 1];
            if header.ends_with("/>") {
                bodies.push(String::new());
                index = at + tag_end + 1;
                continue;
            }
            let body_at = at + tag_end + 1;
            let Some(close_at) = xml[body_at..].find(&close) else {
                break;
            };
            bodies.push(xml[body_at..body_at + close_at].to_string());
            index = body_at + close_at + close.len();
            continue;
        }
        index = at + 1;
    }
    bodies
}

fn is_open_tag(xml: &str, index: usize, name: &str) -> bool {
    let Some(rest) = xml[index..].strip_prefix('<') else {
        return false;
    };
    if rest.starts_with(['/', '!', '?']) {
        return false;
    }
    let rest = match rest.find([':', ' ', '>', '/', '\t', '\n', '\r']) {
        Some(colon) if rest.as_bytes()[colon] == b':' => &rest[colon + 1..],
        _ => rest,
    };
    let Some(rest) = rest.strip_prefix(name) else {
        return false;
    };
    rest.starts_with([' ', '>', '/', '\t', '\n', '\r'])
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let mut rest = tag;
    while let Some(found) = rest.find(&key) {
        let boundary = found == 0 || rest.as_bytes()[found - 1].is_ascii_whitespace();
        let value_at = found + key.len();
        if boundary {
            let end = rest[value_at..].find('"')?;
            return Some(unescape(&rest[value_at..value_at + end]));
        }
        rest = &rest[value_at..];
    }
    None
}

/// Relationship id on `<sheet r:id="…">`, without treating `sheetId` as `id`.
fn attribute_suffix<'a>(tag: &'a str, name: &str) -> Option<String> {
    let key = format!(":{name}=\"");
    let found = tag.find(&key)?;
    let value_at = found + key.len();
    let end = tag[value_at..].find('"')?;
    Some(unescape(&tag[value_at..value_at + end]))
}

fn unescape(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_53647: &str = "/Users/markwatts/omasheets-corpus/spreadsheet-rl-2026/sample/spreadsheetbench_verified__spreadsheet__1_53647__input.xlsx";

    #[test]
    fn projects_sample_53647_visible_tile() {
        let path = Path::new(SAMPLE_53647);
        if !path.is_file() {
            eprintln!("skipping; sample workbook is absent");
            return;
        }
        let tile = project_xlsx_appearance(
            path,
            VisibleWindow {
                origin_row: 0,
                origin_column: 0,
                rows: 40,
                columns: 16,
            },
        )
        .expect("project appearance");
        assert!(
            !tile.merges.is_empty(),
            "expected merges in spreadsheetbench_verified__spreadsheet__1_53647__input.xlsx"
        );
        let explicit_color = tile
            .colors
            .iter()
            .any(|cell| cell.fill.is_some() || cell.font.is_some());
        let custom_width = tile.column_widths.iter().any(|column| column.custom);
        assert!(
            explicit_color || custom_width,
            "expected an explicit rgb color or a non-default column width"
        );
    }
}
