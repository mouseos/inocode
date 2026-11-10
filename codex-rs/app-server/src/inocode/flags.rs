//! `inocode/flags/*`: reads and writes the `[inocode]` flags in config.toml.

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeFlagsReadResponse;
use codex_app_server_protocol::InocodeFlagsWriteResponse;
use codex_inocode_flags::FLAG_KEYS;
use codex_inocode_flags::InocodeFlags;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeFlagsRead { .. } => {
            Ok(Some(read(&codex_inocode_flags::inocode_home()).into()))
        }
        ClientRequest::InocodeFlagsWrite { params, .. } => {
            write(&codex_inocode_flags::inocode_home(), &params.key, params.value)?;
            Ok(Some(InocodeFlagsWriteResponse {}.into()))
        }
        other => Err(unknown_method(other.method_name())),
    }
}

fn read(home: &Path) -> InocodeFlagsReadResponse {
    let flags: HashMap<String, bool> = InocodeFlags::load_from(home)
        .entries()
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
    InocodeFlagsReadResponse { flags }
}

fn write(
    home: &Path,
    key: &str,
    value: bool,
) -> Result<(), codex_app_server_protocol::JSONRPCErrorError> {
    codex_inocode_flags::write_flag_to(home, key, value).map_err(|err| match err.kind() {
        ErrorKind::InvalidInput => invalid_params(format!(
            "unknown inocode flag `{key}`; expected one of: {}",
            FLAG_KEYS.join(", ")
        )),
        _ => internal_error(format!("failed to write inocode flag `{key}`: {err}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_defaults_then_write_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let all = read(dir.path()).flags;
        assert_eq!(all.len(), 11);
        assert!(all.values().all(|v| *v));

        write(dir.path(), "advisor.enabled", false).expect("write");
        let after = read(dir.path()).flags;
        assert_eq!(after.get("advisor.enabled"), Some(&false));
        assert_eq!(after.get("metrics.enabled"), Some(&true));
    }

    #[test]
    fn unknown_key_is_invalid_params() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = write(dir.path(), "nope.enabled", true).expect_err("error");
        assert_eq!(err.code, crate::error_code::INVALID_PARAMS_ERROR_CODE);
    }
}
