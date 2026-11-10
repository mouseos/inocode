//! Built-in search tools backed by the ZeroSearch library.
//!
//! ZeroSearch needs no API key and no account: it queries public search engines,
//! shopping sites and media sites directly from this process. It is linked in as a
//! library, so nothing runs in the background and nothing is allocated until a tool
//! is called.

mod extension;
mod format;
mod tool;

pub use extension::install;
