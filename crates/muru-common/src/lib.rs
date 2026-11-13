//! Shared pieces of Muru used by several crates: the writing of Muru's own
//! log lines and the times on them, process titles, and the names
//! workers go by.

#[cfg(feature = "log")]
pub mod log;
pub mod mounts;
pub mod title;
pub mod worker;
