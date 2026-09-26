//! `WorkbookPort`: how `omasheets.Spreadsheet` reads and writes workbook bytes.

use futures::future::BoxFuture;
use loom_gpui::Port;
use std::fmt;

/// Opaque version the consumer's store assigned to one workbook snapshot.
pub type WorkbookVersion = String;

/// Bytes and metadata from a successful open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbookRead {
    pub bytes: Vec<u8>,
    pub version: WorkbookVersion,
    /// Typically an OOXML spreadsheet media type.
    pub content_type: String,
}

/// A conditional write of workbook bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbookDraft {
    pub expected_version: WorkbookVersion,
    pub bytes: Vec<u8>,
}

/// A port call the control awaits on the GPUI executor. Dropping it cancels.
pub type PortFuture<T> = BoxFuture<'static, Result<T, PortError>>;

/// Why a port call did not succeed (Loom decision 001, port rule 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortError {
    Conflict { current: WorkbookVersion },
    Unavailable,
    Rejected(String),
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { current } => write!(f, "conflict; store has version {current}"),
            Self::Unavailable => f.write_str("workbook store unavailable"),
            Self::Rejected(message) => write!(f, "rejected: {message}"),
        }
    }
}

impl std::error::Error for PortError {}

/// The consumer's workbook store as `omasheets.Spreadsheet` sees it.
///
/// `reference` is the bound `workbookRef` prop. The port checks that it belongs
/// to the consumer's current projection before acting.
pub trait WorkbookPort: Send + Sync {
    fn open(&self, reference: &str) -> PortFuture<WorkbookRead>;
    fn commit(&self, reference: &str, draft: WorkbookDraft) -> PortFuture<WorkbookVersion>;
}

impl Port for dyn WorkbookPort {
    const NAME: &'static str = "workbook";
}
