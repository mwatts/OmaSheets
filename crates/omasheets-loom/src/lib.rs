//! `omasheets.Spreadsheet`: the GPUI spreadsheet as a Loom native composite.
//!
//! # Persistence policy (v1)
//!
//! The control opens workbook bytes through [`WorkbookPort`] and shows them
//! with [`omasheets_gpui::SpreadsheetView`]. Formula-bar edits update the
//! in-memory browse engine (xlsx interchange) or the native document and emit
//! `dirtyChange` / `editCommitted`. Durable export of edited packages is not
//! implemented yet, so [`before_unmount`] discards unsaved edits and does not
//! call [`WorkbookPort::commit`]. Agent mutations stay on the OmaSheets
//! service/MCP path outside this composite.
//!
//! Native Ashlar media type is [`NATIVE_MEDIA_TYPE`] (`.omasheets`). OOXML is
//! import/export interchange only.
//!
//! When durable export exists, commit on debounce/unmount should send a
//! [`WorkbookDraft`] with the expected version from the last open.

mod block;
mod control;
mod port;

pub use block::{SPREADSHEET_BLOCK, install as install_block};
pub use control::{
    Options, SPREADSHEET, SpreadsheetControl, declaration, init, register, register_with,
};
pub use omasheets_gpui::{
    NATIVE_MEDIA_TYPE, ODS_MEDIA_TYPE, XLS_MEDIA_TYPE, XLSX_MEDIA_TYPE, is_native_media_type,
    is_spreadsheet_media_type,
};
pub use port::{PortError, PortFuture, WorkbookDraft, WorkbookPort, WorkbookRead, WorkbookVersion};
