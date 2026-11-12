//! Shared extraction engine used by both the CLI and the Tauri GUI.
//!
//! It drives the external `7zz` and `unrar` binaries, detects multi-volume
//! archives and reports progress to the caller.

pub mod archive;
pub mod engine;

pub use archive::{collect_volumes, detect_kind, ArchiveKind};
pub use engine::{extract, ExtractEvent, ExtractOptions, ToolLocator};
