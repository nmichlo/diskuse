//! The `diskuse` command: its arguments, the full-screen browser, and
//! `scan` and `show`. The library it is built on is `diskuse-core`.
//!
//! [`cli`] is the whole command, for the binary and for the Python
//! package's console script. The rest is public only for the command's
//! own tests.

#![deny(unsafe_code)]

mod app;
mod browse;
mod cli;
mod guide;
mod logfile;
#[doc(hidden)]
pub mod reveal;
mod style;
mod volume_list;

pub use cli::cli;
#[doc(hidden)]
pub use {
    app::{App, Preflight, browse},
    browse::{Browser, Env},
};
