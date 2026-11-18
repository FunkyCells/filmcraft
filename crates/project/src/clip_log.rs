//! The Clip Log: what the editor noted while watching raw footage.
//!
//! Each raw media item can carry any number of [`LogEntry`] rows (a time range of the file, what
//! happens in it, whether it belongs in the cut, and where in the cut it goes). Entries live in
//! [`Project::logs`](crate::Project::logs), keyed by the media item.

use filmcraft_time::Tick;
use serde::{Deserialize, Serialize};

use crate::LogEntryId;

/// The longest `content` or `notes` text of one entry, in bytes.
pub const MAX_LOG_TEXT_BYTES: usize = 65_536;
/// The most entries one media item may carry.
pub const MAX_LOG_ENTRIES_PER_ITEM: usize = 100_000;

/// One logged range of a media file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: LogEntryId,
    /// First tick of the range, in media time.
    pub start: Tick,
    /// End of the range (exclusive), in media time. Always after `start`.
    pub end: Tick,
    /// What the range shows or says.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    /// Whether the range goes into the edit: `None` = not decided yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<bool>,
    /// Position in the edit (1 = first). Not required to be unique; ties keep file and time order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
    /// Free notes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}
