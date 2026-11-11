//! `inocode/params/read` and `inocode/params/write`: the non-boolean `[inocode]` settings.

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeParam;
use codex_app_server_protocol::InocodeParamKind;
use codex_app_server_protocol::InocodeParamValue;
use codex_app_server_protocol::InocodeParamsReadResponse;
use codex_app_server_protocol::InocodeParamsWriteResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_inocode_flags as flags;
use std::io::ErrorKind;
use std::path::Path;

fn to_wire(v: flags::ParamValue) -> InocodeParamValue {
    match v {
        flags::ParamValue::Int(i) => InocodeParamValue::Number(i as f64),
        flags::ParamValue::Float(f) => InocodeParamValue::Number(f),
        flags::ParamValue::Text(s) => InocodeParamValue::Text(s),
    }
}

fn from_wire(spec: &flags::ParamSpec, v: InocodeParamValue) -> flags::ParamValue {
    match (spec.kind, v) {
        (flags::ParamKind::Int, InocodeParamValue::Number(n)) if n.fract() == 0.0 => {
            flags::ParamValue::Int(n as i64)
        }
        (_, InocodeParamValue::Number(n)) => flags::ParamValue::Float(n),
        (_, InocodeParamValue::Text(s)) => flags::ParamValue::Text(s),
    }
}

pub(crate) fn read(home: &Path) -> InocodeParamsReadResponse {
    let params = flags::read_all_from(home)
        .into_iter()
        .map(|(spec, value)| InocodeParam {
            key: spec.key.to_string(),
            kind: match spec.kind {
                flags::ParamKind::Int => InocodeParamKind::Int,
                flags::ParamKind::Float => InocodeParamKind::Float,
                flags::ParamKind::String => InocodeParamKind::String,
                flags::ParamKind::Enum => InocodeParamKind::Enum,
            },
            value: to_wire(value),
            default: to_wire(spec.default_value()),
            options: (!spec.options.is_empty())
                .then(|| spec.options.iter().map(|s| (*s).to_string()).collect()),
        })
        .collect();
    InocodeParamsReadResponse { params }
}

pub(crate) fn write(
    home: &Path,
    key: &str,
    value: Option<InocodeParamValue>,
) -> Result<(), JSONRPCErrorError> {
    let spec = flags::spec(key).ok_or_else(|| {
        invalid_params(format!(
            "unknown inocode parameter `{key}`; expected one of: {}",
            flags::PARAMS
                .iter()
                .map(|p| p.key)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    let value = value.map(|v| from_wire(spec, v));
    flags::write_param_to(home, key, value).map_err(|err| match err.kind() {
        ErrorKind::InvalidInput => invalid_params(err.to_string()),
        _ => internal_error(format!("failed to write inocode parameter `{key}`: {err}")),
    })
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeParamsRead { .. } => {
            Ok(Some(read(&flags::inocode_home()).into()))
        }
        ClientRequest::InocodeParamsWrite { params, .. } => {
            write(&flags::inocode_home(), &params.key, params.value)?;
            Ok(Some(InocodeParamsWriteResponse {}.into()))
        }
        other => Err(unknown_method(other.method_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_reset_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let h = dir.path();
        let get = |key: &str| {
            read(h)
                .params
                .into_iter()
                .find(|p| p.key == key)
                .unwrap()
                .value
        };
        assert_eq!(get("advisor.same_failure_k"), InocodeParamValue::Number(3.0));
        write(h, "advisor.same_failure_k", Some(InocodeParamValue::Number(6.0))).unwrap();
        assert_eq!(get("advisor.same_failure_k"), InocodeParamValue::Number(6.0));
        write(h, "team.fork_turns_default", Some(InocodeParamValue::Text("all".into()))).unwrap();
        assert_eq!(get("team.fork_turns_default"), InocodeParamValue::Text("all".into()));
        write(h, "advisor.same_failure_k", None).unwrap();
        assert_eq!(get("advisor.same_failure_k"), InocodeParamValue::Number(3.0));
        assert!(write(h, "nope.x", Some(InocodeParamValue::Number(1.0))).is_err());
        assert!(write(h, "advisor.repeat_n", Some(InocodeParamValue::Number(1.5))).is_err());
    }
}
