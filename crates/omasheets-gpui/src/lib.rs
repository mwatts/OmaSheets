//! Embeddable spreadsheet surface for gpui-kit hosts.
//!
//! [`SpreadsheetSession`] applies [`omasheets_core::Command`] and reads
//! calculated values from the document. [`SpreadsheetView`] is the GPUI
//! entity a host subscribes to. Workbook styles are read from the xlsx
//! package by [`project_xlsx_appearance`]; the document model stays free of
//! GPUI types.

mod appearance;
mod browse;
mod format;
mod session;
mod view;

pub use appearance::{
    AppearanceError, AppearanceTile, CellColor, ColumnWidth, MergeRect, RgbColor, VisibleWindow,
    project_xlsx_appearance,
};
pub use browse::LoadError;
pub use session::SpreadsheetSession;
pub use view::{SpreadsheetUiEvent, SpreadsheetView};

/// Calculation stays in the owned engine. This crate does not call workbook
/// methods; [`omasheets_xlsx::import_xlsx`] remains the value importer.
#[allow(dead_code)]
fn engine_boundary() {
    let _ = std::any::type_name::<omasheets_calc::Workbook>();
    let _ = omasheets_xlsx::import_xlsx;
}
