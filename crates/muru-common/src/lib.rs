//! Shared pieces of Muru used by several crates: the writing of Muru's own
//! log lines and the times on them, and process titles.

#[cfg(feature = "log")]
pub mod log;
pub mod title;
