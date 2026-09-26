//! Excel number-format display for the grid.
//!
//! The calculation engine stores serials and plain numbers. This module turns
//! those numbers into the text Excel would paint from the cell's format code.
//! It does not read a clock.

use omasheets_calc::serial_date::{DateSystem, civil_from_serial_in, serial_from_number_in};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Text and the format color Excel would paint for this number.
///
/// `color` is `0xRRGGBB` from `[Red]` or `[Color n]`. A section without a
/// color leaves the cell's font alone.
pub struct PaintedNumber {
    pub text: String,
    pub color: Option<u32>,
}

/// Formats `value` with an Excel format code. An empty code is General.
pub fn format_number_with_code(value: f64, code: &str) -> String {
    paint_number(value, code).text
}

/// Formats `value` and reports the color named by the chosen format section.
pub fn paint_number(value: f64, code: &str) -> PaintedNumber {
    paint_number_in(DateSystem::Excel1900, value, code)
}

/// Formats `value` in the workbook's date system.
pub fn paint_number_in(system: DateSystem, value: f64, code: &str) -> PaintedNumber {
    let code = code.trim();
    if code.is_empty() || code.eq_ignore_ascii_case("General") || !value.is_finite() {
        return PaintedNumber {
            text: general_number(value),
            color: None,
        };
    }
    if is_date_code(code) {
        return PaintedNumber {
            text: format_date(system, value, code),
            color: first_color(code),
        };
    }
    let sections = split_sections(code);
    let (section, negative_section) = if value < 0.0 {
        (
            sections.get(1).copied().unwrap_or(sections[0]),
            sections.len() >= 2,
        )
    } else if value == 0.0 {
        (sections.get(2).copied().unwrap_or(sections[0]), false)
    } else {
        (sections[0], false)
    };
    let color = first_color(section);
    if !section_has_digit(section) {
        return PaintedNumber {
            text: literals_only(section),
            color,
        };
    }
    PaintedNumber {
        text: render_number(value, section, value < 0.0 && !negative_section),
        color,
    }
}

pub fn general_number(number: f64) -> String {
    if !number.is_finite() {
        return String::new();
    }
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

fn builtin_format(id: u32) -> Option<&'static str> {
    Some(match id {
        0 => "General",
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        14 => "mm/dd/yyyy",
        37 => "#,##0_);(#,##0)",
        38 => "#,##0_);[Red](#,##0)",
        39 => "#,##0.00_);(#,##0.00)",
        40 => "#,##0.00_);[Red](#,##0.00)",
        49 => "@",
        _ => return None,
    })
}

/// Number format codes and row heights (points) keyed by worksheet name.
pub struct WorkbookLook {
    pub formats: HashMap<(String, u32, u32), String>,
    pub row_points: HashMap<(String, u32), f64>,
}

/// Reads format codes and custom row heights. A missing style part yields an
/// empty look, and the grid then uses General and the default row.
pub fn read_workbook_look(path: &Path) -> WorkbookLook {
    let Ok(file) = File::open(path) else {
        return WorkbookLook::default();
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return WorkbookLook::default();
    };
    let styles = read_zip(&mut archive, "xl/styles.xml").unwrap_or_default();
    let workbook = read_zip(&mut archive, "xl/workbook.xml").unwrap_or_default();
    let formats_by_style = style_formats(&styles);
    let mut look = WorkbookLook::default();
    let names = sheet_names(&workbook);
    for (index, name) in names.iter().enumerate() {
        let part = format!("xl/worksheets/sheet{}.xml", index + 1);
        let Ok(xml) = read_zip(&mut archive, &part) else {
            continue;
        };
        scan_sheet(&xml, name, &formats_by_style, &mut look);
    }
    look
}

impl Default for WorkbookLook {
    fn default() -> Self {
        Self {
            formats: HashMap::new(),
            row_points: HashMap::new(),
        }
    }
}

fn style_formats(styles: &str) -> Vec<String> {
    let mut custom = HashMap::new();
    let mut rest = styles;
    while let Some(start) = rest.find("<numFmt ") {
        let tag_end = rest[start..]
            .find('>')
            .map(|end| start + end)
            .unwrap_or(rest.len());
        let tag = &rest[start..tag_end];
        if let (Some(id), Some(code)) = (attr_u32(tag, "numFmtId"), attr(tag, "formatCode")) {
            custom.insert(id, unescape(&code));
        }
        rest = &rest[tag_end..];
    }
    let xfs = section(styles, "cellXfs");
    let mut formats = Vec::new();
    rest = xfs;
    while let Some(start) = rest.find("<xf ") {
        let tag_end = rest[start..]
            .find('>')
            .map(|end| start + end)
            .unwrap_or(rest.len());
        let tag = &rest[start..tag_end];
        let id = attr_u32(tag, "numFmtId").unwrap_or(0);
        let code = custom
            .get(&id)
            .cloned()
            .or_else(|| builtin_format(id).map(str::to_string))
            .unwrap_or_else(|| "General".to_string());
        formats.push(code);
        rest = &rest[tag_end..];
    }
    formats
}

fn scan_sheet(xml: &str, sheet: &str, formats: &[String], look: &mut WorkbookLook) {
    let mut rest = xml;
    while let Some(start) = rest.find("<row ") {
        let tag_end = rest[start..]
            .find('>')
            .map(|end| start + end)
            .unwrap_or(rest.len());
        let tag = &rest[start..tag_end];
        if let (Some(row), Some(height)) = (attr_u32(tag, "r"), attr_f64(tag, "ht")) {
            if height.is_finite() && height > 0.0 {
                look.row_points
                    .insert((sheet.to_string(), row.saturating_sub(1)), height);
            }
        }
        let row_end = rest[tag_end..]
            .find("</row>")
            .map(|end| tag_end + end)
            .unwrap_or(rest.len());
        let body = &rest[tag_end..row_end];
        let row = attr_u32(tag, "r").unwrap_or(0);
        if row > 0 {
            scan_cells(body, sheet, row - 1, formats, look);
        }
        rest = &rest[row_end..];
    }
}

fn scan_cells(xml: &str, sheet: &str, row: u32, formats: &[String], look: &mut WorkbookLook) {
    let mut rest = xml;
    while let Some(start) = rest.find("<c ") {
        let tag_end = rest[start..]
            .find('>')
            .map(|end| start + end)
            .unwrap_or(rest.len());
        let tag = &rest[start..tag_end];
        if let (Some(reference), Some(style)) = (attr(tag, "r"), attr_u32(tag, "s")) {
            if let Some(code) = formats.get(style as usize) {
                if !code.eq_ignore_ascii_case("General") {
                    if let Some(column) = column_index(&reference) {
                        look.formats
                            .insert((sheet.to_string(), row, column), code.clone());
                    }
                }
            }
        }
        rest = &rest[tag_end..];
    }
}

fn sheet_names(workbook: &str) -> Vec<String> {
    let head = workbook.split("<definedNames>").next().unwrap_or(workbook);
    let mut names = Vec::new();
    let mut rest = head;
    while let Some(start) = rest.find("<sheet ") {
        let tag_end = rest[start..]
            .find('>')
            .map(|end| start + end)
            .unwrap_or(rest.len());
        let tag = &rest[start..tag_end];
        if let Some(name) = attr(tag, "name") {
            names.push(unescape(&name));
        }
        rest = &rest[tag_end..];
    }
    names
}

fn section<'a>(xml: &'a str, name: &str) -> &'a str {
    let open = format!("<{name}");
    let Some(start) = xml.find(&open) else {
        return "";
    };
    let close = format!("</{name}>");
    let end = xml[start..]
        .find(&close)
        .map(|offset| start + offset)
        .unwrap_or(xml.len());
    &xml[start..end]
}

fn read_zip(archive: &mut zip::ZipArchive<File>, name: &str) -> Result<String, ()> {
    let mut part = archive.by_name(name).map_err(|_| ())?;
    let mut text = String::new();
    part.read_to_string(&mut text).map_err(|_| ())?;
    Ok(text)
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

fn attr_u32(tag: &str, name: &str) -> Option<u32> {
    attr(tag, name)?.parse().ok()
}

fn attr_f64(tag: &str, name: &str) -> Option<f64> {
    attr(tag, name)?.parse().ok()
}

fn unescape(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn column_index(reference: &str) -> Option<u32> {
    let mut column = 0_u32;
    let mut saw = false;
    for byte in reference.bytes() {
        if byte.is_ascii_alphabetic() {
            saw = true;
            column = column
                .saturating_mul(26)
                .saturating_add(u32::from(byte.to_ascii_uppercase() - b'A' + 1));
        } else {
            break;
        }
    }
    saw.then(|| column.saturating_sub(1))
}

fn is_date_code(code: &str) -> bool {
    let mut quoted = false;
    let mut bracket = false;
    let mut letters = String::new();
    for character in code.chars() {
        match character {
            '"' if !bracket => quoted = !quoted,
            '[' if !quoted => bracket = true,
            ']' if !quoted => bracket = false,
            _ if !quoted && !bracket => letters.push(character.to_ascii_lowercase()),
            _ => {}
        }
    }
    letters.contains('y') || letters.contains('d') || letters.contains('h')
}

/// Excel's `[Red]` and `[Color n]` palette. Named green is the dark format
/// color; `[Color 4]` is the bright palette green.
fn first_color(section: &str) -> Option<u32> {
    let mut quoted = false;
    let mut chars = section.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' => quoted = !quoted,
            '[' if !quoted => {
                let mut token = String::new();
                for inner in chars.by_ref() {
                    if inner == ']' {
                        break;
                    }
                    token.push(inner);
                }
                if let Some(color) = format_color(&token) {
                    return Some(color);
                }
            }
            _ => {}
        }
    }
    None
}

fn format_color(token: &str) -> Option<u32> {
    let token = token.trim();
    let lower = token.to_ascii_lowercase();
    if let Some(index) = lower.strip_prefix("color") {
        let index: usize = index.trim().parse().ok()?;
        return INDEXED_COLORS.get(index.wrapping_sub(1)).copied();
    }
    Some(match lower.as_str() {
        "black" => 0x0000_00,
        "white" => 0xFFFF_FF,
        "red" => 0xFF00_00,
        "green" => 0x0080_00,
        "blue" => 0x0000_FF,
        "yellow" => 0xFFFF_00,
        "magenta" => 0xFF00_FF,
        "cyan" => 0x00FF_FF,
        _ => return None,
    })
}

/// Default workbook palette. `[Color n]` is 1-based.
const INDEXED_COLORS: [u32; 56] = [
    0x0000_00, 0xFFFF_FF, 0xFF00_00, 0x00FF_00, 0x0000_FF, 0xFFFF_00, 0xFF00_FF, 0x00FF_FF,
    0x0000_00, 0xFFFF_FF, 0xFF00_00, 0x00FF_00, 0x0000_FF, 0xFFFF_00, 0xFF00_FF, 0x00FF_FF,
    0x8000_00, 0x0080_00, 0x0000_80, 0x8080_00, 0x8000_80, 0x0080_80, 0xC0C0_C0, 0x8080_80,
    0x9999_FF, 0x9933_66, 0xFFFF_CC, 0xCCFF_FF, 0x6600_66, 0xFF80_80, 0x0066_CC, 0xCCCC_FF,
    0x0000_80, 0xFF00_FF, 0xFFFF_00, 0x00FF_FF, 0x8000_80, 0x8000_00, 0x0080_80, 0x0000_FF,
    0x00CC_FF, 0xCCFF_FF, 0xCCFF_CC, 0xFFFF_99, 0x99CC_FF, 0xFF99_CC, 0xCC99_FF, 0xFFCC_99,
    0x3366_FF, 0x33CC_CC, 0x99CC_00, 0xFFCC_00, 0xFF99_00, 0xFF66_00, 0x6666_99, 0x9696_96,
];

fn format_date(system: DateSystem, value: f64, code: &str) -> String {
    let Ok(serial) = serial_from_number_in(system, value) else {
        return general_number(value);
    };
    let Ok(date) = civil_from_serial_in(system, serial) else {
        return general_number(value);
    };
    let mut out = String::new();
    let chars: Vec<char> = code.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '"' {
            index += 1;
            while index < chars.len() && chars[index] != '"' {
                out.push(chars[index]);
                index += 1;
            }
            index += 1;
            continue;
        }
        if chars[index] == '\\' {
            index += 1;
            if index < chars.len() {
                out.push(chars[index]);
                index += 1;
            }
            continue;
        }
        let start = index;
        let kind = chars[index].to_ascii_lowercase();
        if matches!(kind, 'y' | 'm' | 'd' | 'h' | 's') {
            while index < chars.len() && chars[index].eq_ignore_ascii_case(&chars[start]) {
                index += 1;
            }
            let width = index - start;
            match kind {
                'y' if width >= 3 => out.push_str(&date.year.to_string()),
                'y' => out.push_str(&format!("{:02}", date.year.rem_euclid(100))),
                'm' if width >= 3 => {
                    let month = date.month.clamp(1, 12) as usize;
                    out.push_str(MONTHS[month - 1]);
                }
                'm' if width >= 2 => out.push_str(&format!("{:02}", date.month)),
                'm' => out.push_str(&date.month.to_string()),
                'd' if width >= 2 => out.push_str(&format!("{:02}", date.day)),
                'd' => out.push_str(&date.day.to_string()),
                'h' | 's' => {}
                _ => {}
            }
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}

fn split_sections(code: &str) -> Vec<&str> {
    let mut sections = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (index, character) in code.char_indices() {
        if character == '"' {
            quoted = !quoted;
        } else if character == ';' && !quoted {
            sections.push(&code[start..index]);
            start = index + character.len_utf8();
        }
    }
    sections.push(&code[start..]);
    if sections.is_empty() {
        sections.push(code);
    }
    sections
}

fn section_has_digit(section: &str) -> bool {
    let mut quoted = false;
    for character in section.chars() {
        if character == '"' {
            quoted = !quoted;
        } else if !quoted && matches!(character, '0' | '#' | '?') {
            return true;
        }
    }
    false
}

fn literals_only(section: &str) -> String {
    let mut out = String::new();
    let mut chars = section.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' => {
                for quoted in chars.by_ref() {
                    if quoted == '"' {
                        break;
                    }
                    out.push(quoted);
                }
            }
            '\\' => {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            }
            '_' => {
                chars.next();
                out.push(' ');
            }
            '*' => {
                chars.next();
            }
            '[' => {
                for skipped in chars.by_ref() {
                    if skipped == ']' {
                        break;
                    }
                }
            }
            ';' => break,
            _ => out.push(character),
        }
    }
    out
}

struct ScientificPicture {
    digits_before: usize,
    decimals: usize,
    exponent_digits: usize,
    /// `E+` always prints a sign. `E-` prints a sign only when the exponent is negative.
    always_sign: bool,
    marker: char,
}

fn render_number(value: f64, section: &str, force_minus: bool) -> String {
    let mut quoted = false;
    let mut percent = false;
    let mut thousands = false;
    let mut seen_dot = false;
    let mut digits_before = 0_usize;
    let mut decimals = 0_usize;
    let mut scientific: Option<ScientificPicture> = None;
    let mut parens = false;
    let mut prefix = String::new();
    let mut suffix = String::new();
    let mut number_started = false;
    let mut chars = section.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '"' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            if number_started {
                suffix.push(character);
            } else {
                prefix.push(character);
            }
            continue;
        }
        match character {
            '\\' => {
                if let Some(escaped) = chars.next() {
                    if number_started {
                        suffix.push(escaped);
                    } else {
                        prefix.push(escaped);
                    }
                    if escaped == '(' || escaped == ')' {
                        parens = true;
                    }
                }
            }
            '[' => {
                for skipped in chars.by_ref() {
                    if skipped == ']' {
                        break;
                    }
                }
            }
            '_' => {
                chars.next();
                if number_started {
                    suffix.push(' ');
                } else {
                    prefix.push(' ');
                }
            }
            '*' => {
                chars.next();
            }
            '%' => {
                percent = true;
                suffix.push('%');
                number_started = true;
            }
            '.' => {
                seen_dot = true;
                number_started = true;
            }
            ',' => {
                if number_started && scientific.is_none() {
                    thousands = true;
                }
            }
            '0' | '#' | '?' => {
                number_started = true;
                if seen_dot {
                    decimals += 1;
                } else {
                    digits_before += 1;
                }
            }
            'E' | 'e'
                if chars
                    .peek()
                    .is_some_and(|next| matches!(next, '+' | '-' | '0' | '#' | '?')) =>
            {
                let always_sign = chars.peek().is_some_and(|next| *next == '+');
                if chars.peek().is_some_and(|next| matches!(next, '+' | '-')) {
                    chars.next();
                }
                let mut exponent_digits = 0_usize;
                while chars
                    .peek()
                    .is_some_and(|next| matches!(next, '0' | '#' | '?'))
                {
                    chars.next();
                    exponent_digits += 1;
                }
                scientific = Some(ScientificPicture {
                    digits_before: digits_before.max(1),
                    decimals,
                    exponent_digits,
                    always_sign,
                    marker: character,
                });
                number_started = true;
            }
            '(' | ')' => {
                parens = true;
                if number_started {
                    suffix.push(character);
                } else {
                    prefix.push(character);
                }
            }
            _ => {
                if number_started {
                    suffix.push(character);
                } else {
                    prefix.push(character);
                }
            }
        }
    }
    let mut magnitude = value.abs();
    if percent {
        magnitude *= 100.0;
    }
    let body = if let Some(picture) = scientific {
        format_scientific(magnitude, &picture)
    } else {
        format_fixed(magnitude, decimals, thousands)
    };
    if parens && value < 0.0 {
        format!("{prefix}{body}{suffix}")
    } else if force_minus && value < 0.0 {
        format!("{prefix}-{body}{suffix}")
    } else {
        format!("{prefix}{body}{suffix}")
    }
}

/// `0.00E+00` is `1.23E+03`. The exponent has enough digits that the mantissa
/// shows `digits_before` digits to the left of the decimal, and `E+` always
/// prints the exponent sign.
fn format_scientific(value: f64, picture: &ScientificPicture) -> String {
    let digits_before = picture.digits_before.max(1);
    if value == 0.0 {
        return format!(
            "{}{}",
            format_mantissa(0.0, digits_before, picture.decimals),
            format_exponent(0, picture)
        );
    }
    let mut exponent = decade(value) - (digits_before as i32 - 1);
    let mantissa = value / 10_f64.powi(exponent);
    let mut rounded = round_places(mantissa, picture.decimals);
    let limit = 10_f64.powi(digits_before as i32);
    if rounded >= limit {
        rounded /= 10.0;
        exponent += 1;
    }
    format!(
        "{}{}",
        format_mantissa(rounded, digits_before, picture.decimals),
        format_exponent(exponent, picture)
    )
}

fn decade(value: f64) -> i32 {
    let mut exponent = value.log10().floor() as i32;
    let scaled = value / 10_f64.powi(exponent);
    if scaled >= 10.0 {
        exponent += 1;
    } else if scaled < 1.0 {
        exponent -= 1;
    }
    exponent
}

fn format_mantissa(value: f64, digits_before: usize, decimals: usize) -> String {
    let mut text = format_fixed(value, decimals, false);
    let whole_len = text.split('.').next().map_or(text.len(), str::len);
    if whole_len < digits_before {
        text.insert_str(0, &"0".repeat(digits_before - whole_len));
    }
    text
}

fn format_exponent(exponent: i32, picture: &ScientificPicture) -> String {
    let sign = if exponent < 0 {
        "-"
    } else if picture.always_sign {
        "+"
    } else {
        ""
    };
    let digits = exponent.unsigned_abs().to_string();
    let digits = if picture.exponent_digits > digits.len() {
        format!("{:0>width$}", digits, width = picture.exponent_digits)
    } else {
        digits
    };
    format!("{}{sign}{digits}", picture.marker)
}

/// Half away from zero. A tiny nudge keeps `9.995` at two places from landing
/// just below the binary halfway point and rounding down.
fn round_places(value: f64, decimals: usize) -> f64 {
    if decimals == 0 {
        return value.round();
    }
    let scale = 10_f64.powi(decimals as i32);
    let scaled = value * scale;
    let nudged = scaled + scaled.signum() * 1e-8;
    nudged.round() / scale
}

fn format_fixed(value: f64, decimals: usize, thousands: bool) -> String {
    let scale = 10_f64.powi(decimals as i32);
    let rounded = if decimals == 0 {
        value.round()
    } else {
        (value * scale).round() / scale
    };
    let mut text = format!("{rounded:.decimals$}");
    if thousands {
        if let Some((whole, fraction)) = text.split_once('.') {
            text = format!("{}.{}", group_thousands(whole), fraction);
        } else {
            text = group_thousands(&text);
        }
    }
    text
}

fn group_thousands(whole: &str) -> String {
    let digits: Vec<char> = whole.chars().collect();
    let mut out = String::new();
    for (index, digit) in digits.iter().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use omasheets_calc::serial_date::{civil_from_serial, serial_from_civil};

    #[test]
    fn percent_with_one_decimal_multiplies_by_one_hundred() {
        assert_eq!(format_number_with_code(0.065, "0.0%"), "6.5%");
    }

    #[test]
    fn date_serial_uses_the_excel_civil_date() {
        let serial = serial_from_civil(2023, 2, 3).unwrap();
        let date = civil_from_serial(serial).unwrap();
        let text = format_number_with_code(serial as f64, "mm/dd/yyyy");
        assert_eq!(
            text,
            format!("{:02}/{:02}/{}", date.month, date.day, date.year)
        );
        assert_eq!(text, "02/03/2023");
    }

    #[test]
    fn accounting_negative_uses_parentheses_and_zero_uses_the_dash() {
        assert_eq!(
            format_number_with_code(-1234.0, "#,##0_);(#,##0)"),
            "(1,234)"
        );
        assert_eq!(
            format_number_with_code(0.0, "#,##0.0_);(#,##0.0_);\"-\""),
            "-"
        );
        assert_eq!(format_number_with_code(2024.0, "\"FY\"0"), "FY2024");
    }

    #[test]
    fn scientific_picture_matches_excel() {
        assert_eq!(format_number_with_code(1234.0, "0.00E+00"), "1.23E+03");
        assert_eq!(format_number_with_code(0.00123, "0.00E+00"), "1.23E-03");
        assert_eq!(format_number_with_code(0.0, "0.00E+00"), "0.00E+00");
        assert_eq!(format_number_with_code(1000.0, "0.00E+00"), "1.00E+03");
        assert_eq!(format_number_with_code(1234.0, "0.00E-00"), "1.23E03");
        assert_eq!(format_number_with_code(12345.0, "00.00E+00"), "12.35E+03");
        assert_eq!(format_number_with_code(999.5, "0.00E+00"), "1.00E+03");
    }

    #[test]
    fn format_colors_paint_the_section_that_applies() {
        let negative = paint_number(-1234.0, "#,##0_);[Red](#,##0)");
        assert_eq!(negative.text, "(1,234)");
        assert_eq!(negative.color, Some(0xFF0000));
        let positive = paint_number(1234.0, "#,##0_);[Red](#,##0)");
        assert_eq!(positive.text, "1,234 ");
        assert_eq!(positive.color, None);
        let scientific = paint_number(1234.0, "[Red]0.00E+00");
        assert_eq!(scientific.text, "1.23E+03");
        assert_eq!(scientific.color, Some(0xFF0000));
        assert_eq!(paint_number(1.0, "[Color 3]0").color, Some(0xFF0000));
        assert_eq!(paint_number(1.0, "[Green]0").color, Some(0x008000));
        assert_eq!(paint_number(1.0, "[Color 4]0").color, Some(0x00FF00));
        assert_eq!(format_number_with_code(12.0, "[Red]0.00"), "12.00");
    }
}
