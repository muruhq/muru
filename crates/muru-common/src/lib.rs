//! Shared pieces of Muru used by several crates: the writing of Muru's own
//! log lines and the times on them, process titles, the names workers go
//! by, files written whole for other processes to read, and files opened
//! beneath a root directory.

pub mod beneath;
pub mod files;
#[cfg(feature = "log")]
pub mod log;
pub mod mounts;
pub mod title;
pub mod worker;
