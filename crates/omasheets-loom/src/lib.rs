//! `omasheets.Spreadsheet`: the GPUI spreadsheet as a Loom native composite.
//!
//! # Persistence policy (v1)
//!
//! The control opens workbook bytes through [`WorkbookPort`] and shows them
//! with [`omasheets_gpui::SpreadsheetView`]. Formula-bar edits update the
//! in-memory browse engine and emit `dirtyChange` / `editCommitted`. Durable
//! export of edited packages is not implemented yet, so [`before_unmount`]
//! discards unsaved edits and does not call [`WorkbookPort::commit`]. Agent
//! mutations stay on the OmaSheets service/MCP path outside this composite.
//!
//! When xlsx export exists, commit on debounce/unmount should send a
//! [`WorkbookDraft`] with the expected version from the last open.

mod control;
mod port;

pub use control::{
    SPREADSHEET, SpreadsheetControl, declaration, init, register, register_with, Options,
};
pub use port::{PortError, PortFuture, WorkbookDraft, WorkbookPort, WorkbookRead, WorkbookVersion};
