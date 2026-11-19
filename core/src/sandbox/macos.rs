//! Confinement on macOS. Not yet implemented.

use super::Roots;

pub(super) fn confine(_argv: &[String], _roots: &Roots) -> Result<Vec<String>, String> {
    Err("the macOS sandbox is not implemented yet, so the command was not run".into())
}
