//! dimagine-core: the read-only heart of the dimagine tools.
//!
//! A dimagine library is a folder of images with optional Markdown notes,
//! collections and JSON Canvas boards (docs/FORMAT.md). This crate walks a
//! library, parses image notes, extracts and resolves links, sniffs image
//! headers and computes summaries and findings. It NEVER writes: every
//! function is side-effect-free regarding the library files.
//!
//! Module map (HLD §"Modules" 0.1):
//!
//! - [`library`]: walk and classify (FORMAT §2),
//! - [`note`]: front matter and note properties (FORMAT §3),
//! - [`links`]: extract and resolve `![[...]]`, `[[]]`, `![]()` and canvas
//!   file nodes (FORMAT §5.1, §6),
//! - [`check`]: findings for `dimagine check`,
//! - [`scan`]: summary for `dimagine scan`.
//!
//! The HLD's `index`, `preview`, `import` and `serve` modules are out of scope
//! for 0.1; this layout leaves them somewhere natural to slot in.

pub mod check;
pub mod format;
pub mod library;
pub mod links;
pub mod note;
pub mod scan;
pub mod sniff;

pub use check::{CheckReport, Finding, Severity};
pub use library::{FileClass, FileEntry, Library, NotAFolder};
pub use scan::ScanReport;
