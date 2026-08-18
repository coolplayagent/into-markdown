//! Testable boundaries shared with repository-owned quality tooling.
#![allow(dead_code)]

mod args;
mod config;
mod error;
mod transaction;

#[doc(hidden)]
pub use config::fuzz_plugin_protocol;
