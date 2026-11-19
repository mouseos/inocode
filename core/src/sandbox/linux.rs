//! Confinement on Linux. Not yet implemented.

use std::path::Path;

use super::Roots;

pub(super) fn confine(
    _argv: &[String],
    _roots: &Roots,
    _runner: &Path,
) -> Result<Vec<String>, String> {
    Err("the Linux sandbox is not implemented yet, so the command was not run".into())
}

pub(super) fn run(_args: &[String]) -> Result<i32, String> {
    Err("the Linux sandbox is not implemented yet".into())
}
