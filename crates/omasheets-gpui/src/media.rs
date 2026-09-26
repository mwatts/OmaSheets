//! Spreadsheet media types for Ashlar File / WorkbookPort content_type.
//!
//! Native documents are `.omasheets` (SQLite event store), not Excel.
//! OOXML remains an import/export interchange type only.

/// Freedesktop / Ashlar media type for a native `.omasheets` document.
pub const NATIVE_MEDIA_TYPE: &str = "application/x-omasheets";

/// OOXML spreadsheet interchange (`.xlsx`).
pub const XLSX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// Legacy Excel binary (import only).
pub const XLS_MEDIA_TYPE: &str = "application/vnd.ms-excel";

/// OpenDocument spreadsheet (import only when a converter exists).
pub const ODS_MEDIA_TYPE: &str = "application/vnd.oasis.opendocument.spreadsheet";

/// True when `media_type` is the native OmaSheets document type.
#[must_use]
pub fn is_native_media_type(media_type: &str) -> bool {
    essence(media_type).eq_ignore_ascii_case(NATIVE_MEDIA_TYPE)
}

/// True when Ashlar should open the blob with the spreadsheet surface.
#[must_use]
pub fn is_spreadsheet_media_type(media_type: &str) -> bool {
    let essence = essence(media_type);
    essence.eq_ignore_ascii_case(NATIVE_MEDIA_TYPE)
        || essence.eq_ignore_ascii_case(XLSX_MEDIA_TYPE)
        || essence.eq_ignore_ascii_case(XLS_MEDIA_TYPE)
        || essence.eq_ignore_ascii_case(ODS_MEDIA_TYPE)
        || essence.eq_ignore_ascii_case("application/vnd.ms-excel.sheet.macroEnabled.12")
}

fn essence(media_type: &str) -> &str {
    media_type.split(';').next().unwrap_or(media_type).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_is_distinct_from_xlsx() {
        assert!(is_native_media_type(NATIVE_MEDIA_TYPE));
        assert!(!is_native_media_type(XLSX_MEDIA_TYPE));
        assert!(is_spreadsheet_media_type(NATIVE_MEDIA_TYPE));
        assert!(is_spreadsheet_media_type(&format!(
            "{XLSX_MEDIA_TYPE}; charset=binary"
        )));
    }
}
