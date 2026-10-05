//! `omasheets.Spreadsheet`: the GPUI spreadsheet as a Loom native composite.
//!
//! # Persistence policy
//!
//! The control opens workbook bytes through [`WorkbookPort`]. Native
//! `.omasheets` documents append formula-bar edits into the store; a debounced
//! (and unmount) flush calls [`WorkbookPort::commit`] with checkpointed bytes.
//! OOXML browse sessions still cannot export; dirty local edits are discarded
//! on unmount with `saveState=unavailable` after a flush attempt.
//!
//! Native Ashlar media type is [`NATIVE_MEDIA_TYPE`] (`.omasheets`). OOXML is
//! import/export interchange only.

mod block;
mod control;
mod port;

pub use block::{
    SPREADSHEET_BLOCK, changed_elsewhere as block_changed_elsewhere, install as install_block,
    install_read_only as install_block_read_only, set_touch as set_block_touch,
};
pub use control::{
    Options, SPREADSHEET, SpreadsheetControl, declaration, init, register, register_with,
};
pub use omasheets_gpui::{
    NATIVE_MEDIA_TYPE, ODS_MEDIA_TYPE, XLS_MEDIA_TYPE, XLSX_MEDIA_TYPE, is_native_media_type,
    is_spreadsheet_media_type, sniff_spreadsheet_media_type,
};
pub use port::{PortError, PortFuture, WorkbookDraft, WorkbookPort, WorkbookRead, WorkbookVersion};
