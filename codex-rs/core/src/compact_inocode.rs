//! InoCode additions to local compaction: re-attaching recently accessed files,
//! a per-thread consecutive-failure circuit breaker, and pinned-rule re-injection.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;

/// Team estimate settings from `team.*` parameters, read at the point of use.
pub(crate) fn estimate_config() -> codex_inocode_team::EstimateConfig {
    use codex_inocode_flags::float_param;
    codex_inocode_team::EstimateConfig {
        threshold: float_param("team.speedup_threshold"),
        split_overhead_secs: float_param("team.split_overhead_secs"),
        integrate_overhead_secs: float_param("team.integrate_overhead_secs"),
        default_task_secs: float_param("team.task_secs"),
        ..Default::default()
    }
}

/// Files re-attached after a local compaction (`compaction.reattach_files`).
pub(crate) fn max_reattached_files() -> usize {
    usize::try_from(codex_inocode_flags::int_param("compaction.reattach_files")).unwrap_or(usize::MAX)
}
/// Total token budget for all re-attached files.
pub(crate) fn max_reattached_tokens() -> usize {
    usize::try_from(codex_inocode_flags::int_param("compaction.reattach_tokens")).unwrap_or(usize::MAX)
}
/// Consecutive auto-compaction failures after which the session stops auto-compacting.
pub(crate) fn max_consecutive_compact_failures() -> u32 {
    codex_inocode_compaction::max_consecutive_failures()
}

/// Working directory of the turn, as a local path.
#[allow(deprecated)]
pub(crate) fn local_cwd(turn: &crate::session::turn_context::TurnContext) -> PathBuf {
    turn.cwd.as_path().to_path_buf()
}

// ---------------------------------------------------------------------------
// Circuit breaker
// ---------------------------------------------------------------------------

fn failures() -> &'static Mutex<HashMap<String, u32>> {
    static MAP: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// True once `thread_id` has failed auto-compaction too many times in a row.
pub(crate) fn auto_compact_tripped(thread_id: &str) -> bool {
    let map = failures().lock().unwrap_or_else(|e| e.into_inner());
    map.get(thread_id)
        .is_some_and(|n| *n >= max_consecutive_compact_failures())
}

pub(crate) fn record_compact_result(thread_id: &str, ok: bool) {
    let mut map = failures().lock().unwrap_or_else(|e| e.into_inner());
    if ok {
        map.remove(thread_id);
    } else {
        *map.entry(thread_id.to_string()).or_insert(0) += 1;
    }
}

fn failure_count(thread_id: &str) -> u32 {
    failures()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(thread_id)
        .copied()
        .unwrap_or(0)
}

/// Reports one finished compaction (`inocode/compaction/event`). Call after
/// [`record_compact_result`] for automatic runs. A failure that reaches the limit is followed
/// by a `breakerTripped` event.
pub(crate) fn emit_compaction_result<E: std::fmt::Debug>(
    thread_id: &str,
    auto: bool,
    reason: &str,
    result: &Result<(), E>,
) {
    use codex_inocode_compaction::CompactionEventKind;
    let failures = failure_count(thread_id);
    match result {
        Ok(()) => {
            codex_inocode_compaction::emit(thread_id, CompactionEventKind::Completed, auto, reason, failures);
        }
        Err(err) => {
            tracing::warn!("compaction failed ({reason}): {err:?}");
            codex_inocode_compaction::emit(thread_id, CompactionEventKind::Failed, auto, reason, failures);
            if auto && failures >= max_consecutive_compact_failures() {
                codex_inocode_compaction::emit(
                    thread_id,
                    CompactionEventKind::BreakerTripped,
                    auto,
                    reason,
                    failures,
                );
            }
        }
    }
}

/// Reports that an automatic compaction was refused because the breaker is open.
pub(crate) fn emit_breaker_open(thread_id: &str, reason: &str) {
    codex_inocode_compaction::emit(
        thread_id,
        codex_inocode_compaction::CompactionEventKind::BreakerTripped,
        true,
        reason,
        failure_count(thread_id),
    );
}

/// Ids of the pinned rules that would be re-injected for `cwd`.
pub(crate) fn pinned_rule_ids(cwd: &Path) -> Vec<String> {
    codex_inocode_compaction::rule_ids_in_text(&codex_inocode_knowledge::pinned_rules_text(cwd))
}

// ---------------------------------------------------------------------------
// Recent files
// ---------------------------------------------------------------------------

const READ_COMMANDS: &[&str] = &[
    "cat",
    "type",
    "get-content",
    "gc",
    "head",
    "tail",
    "bat",
    "less",
    "more",
    "nl",
];

fn clean_token(tok: &str) -> &str {
    tok.trim_matches(|c| c == '"' || c == '\'' || c == ',' || c == '[' || c == ']')
}

/// Paths named by the arguments of one tool call (reads and writes), in order.
fn paths_from_call(item: &ResponseItem) -> Vec<String> {
    let mut out = Vec::new();
    match item {
        ResponseItem::CustomToolCall { name, input, .. } if name == "apply_patch" => {
            for line in input.lines() {
                for prefix in ["*** Add File: ", "*** Update File: "] {
                    if let Some(p) = line.strip_prefix(prefix) {
                        out.push(p.trim().to_string());
                    }
                }
            }
        }
        ResponseItem::FunctionCall {
            name, arguments, ..
        } => {
            if name == "apply_patch" {
                for line in arguments.lines() {
                    for prefix in ["*** Add File: ", "*** Update File: "] {
                        if let Some(idx) = line.find(prefix) {
                            let rest = &line[idx + prefix.len()..];
                            out.push(rest.trim().to_string());
                        }
                    }
                }
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
                return out;
            };
            let command = value
                .get("cmd")
                .or_else(|| value.get("command"))
                .map(|c| match c {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Array(a) => a
                        .iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => String::new(),
                })
                .unwrap_or_default();
            let mut reading = false;
            for tok in command.split_whitespace() {
                if matches!(tok, "|" | ";" | "&&" | "||") {
                    reading = false;
                    continue;
                }
                let tok = clean_token(tok);
                if READ_COMMANDS.contains(&tok.to_ascii_lowercase().as_str()) {
                    reading = true;
                    continue;
                }
                if reading && !tok.starts_with('-') && !tok.is_empty() {
                    out.push(tok.to_string());
                }
            }
        }
        _ => {}
    }
    out
}

/// Most recently touched file paths, newest first, deduplicated.
pub(crate) fn recent_file_paths<'a>(
    items: impl DoubleEndedIterator<Item = &'a ResponseItem>,
    cwd: &Path,
    limit: usize,
) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items.rev() {
        let mut paths = paths_from_call(item);
        paths.reverse();
        for p in paths {
            let path = PathBuf::from(&p);
            let path = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            if seen.insert(path.clone()) {
                out.push(path);
                if out.len() >= limit * 3 {
                    // Over-collect: some of these may no longer exist.
                    return out;
                }
            }
        }
    }
    out
}

/// Reads up to `compaction.reattach_files` of the candidate files within the token budget and renders
/// them as a block appended to the compaction summary. Empty if nothing could be read.
#[cfg(test)]
pub(crate) fn render_reattached_files(candidates: &[PathBuf]) -> String {
    render_reattached_files_with_paths(candidates).0
}

/// Like [`render_reattached_files`], also returning the files that were actually attached.
pub(crate) fn render_reattached_files_with_paths(candidates: &[PathBuf]) -> (String, Vec<String>) {
    let mut remaining = max_reattached_tokens();
    let mut blocks = Vec::new();
    let mut attached = Vec::new();
    for path in candidates {
        if blocks.len() >= max_reattached_files() || remaining == 0 {
            break;
        }
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > 4_000_000 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let tokens = approx_token_count(&text);
        let body = if tokens > remaining {
            truncate_text(&text, TruncationPolicy::Tokens(remaining))
        } else {
            text
        };
        remaining = remaining.saturating_sub(approx_token_count(&body));
        blocks.push(format!("--- {} ---\n{body}", path.display()));
        attached.push(path.display().to_string());
    }
    if blocks.is_empty() {
        return (String::new(), Vec::new());
    }
    (
        format!(
            "Recently accessed files (re-attached after compaction; current content):\n\n{}",
            blocks.join("\n\n")
        ),
        attached,
    )
}

/// Pinned prohibition rules, rendered for re-injection after compaction or a model switch.
pub(crate) fn pinned_rules_block(cwd: &Path) -> String {
    let text = codex_inocode_knowledge::pinned_rules_text(cwd);
    if text.trim().is_empty() {
        return String::new();
    }
    format!("Pinned rules (always apply):\n{}", text.trim_end())
}

/// Pinned rules as a user message to place in the initial context, if any apply.
pub(crate) fn pinned_rules_item(cwd: &Path) -> Option<ResponseItem> {
    let text = pinned_rules_block(cwd);
    if text.is_empty() {
        return None;
    }
    Some(ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_owned(),
        content: vec![codex_protocol::models::ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

/// Items to record at the start of a turn: the `team.auto_select` recommendation for a request
/// that holds several independent tasks, and the pinned rules again after a model switch.
/// Must run before `previous_turn_settings` is overwritten for this turn.
pub(crate) async fn turn_extras(
    sess: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    user_input: &[codex_protocol::user_input::UserInput],
) -> Vec<ResponseItem> {
    let mut items = Vec::new();
    let flags = codex_inocode_flags::InocodeFlags::load();
    if flags.team_auto_select {
        let text: String = user_input
            .iter()
            .filter_map(|u| match u {
                codex_protocol::user_input::UserInput::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("
");
        if let Some((estimate, rec)) = codex_inocode_team::recommend_for_input(
            &text,
            &estimate_config(),
        ) {
            codex_inocode_team::roster_for(&sess.thread_id.to_string()).set_estimate(estimate);
            items.push(user_text_item(rec));
        }
    }
    if let Some(previous) = sess.previous_turn_settings().await
        && previous.model != turn.model_info().slug
    {
        let cwd = local_cwd(turn);
        if let Some(item) = pinned_rules_item(&cwd) {
            items.push(item);
        }
        let task: String = user_input
            .iter()
            .filter_map(|u| match u {
                codex_protocol::user_input::UserInput::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let pack = codex_inocode_knowledge::build_handoff_pack(&cwd, &task);
        if !pack.trim().is_empty() {
            items.push(user_text_item(pack));
        }
    }
    items
}

fn user_text_item(text: String) -> ResponseItem {
    ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_owned(),
        content: vec![codex_protocol::models::ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_trips_after_three_and_resets() {
        let id = "t-breaker";
        assert!(!auto_compact_tripped(id));
        for _ in 0..2 {
            record_compact_result(id, false);
        }
        assert!(!auto_compact_tripped(id));
        record_compact_result(id, false);
        assert!(auto_compact_tripped(id));
        record_compact_result(id, true);
        assert!(!auto_compact_tripped(id));
    }

    fn custom(name: &str, input: &str) -> ResponseItem {
        ResponseItem::CustomToolCall {
            status: None,
            encrypted_content: None,
            id: None,
            call_id: "c".into(),
            name: name.into(),
            namespace: None,
            input: input.into(),
            internal_chat_message_metadata_passthrough: None,
        }
    }

    #[test]
    fn collects_patch_paths_newest_first() {
        let items = vec![
            custom(
                "apply_patch",
                "*** Begin Patch\n*** Update File: a.rs\n*** End Patch",
            ),
            custom("apply_patch", "*** Add File: b.rs\n*** Update File: a.rs"),
        ];
        let got = recent_file_paths(items.iter(), Path::new("/w"), 5);
        assert_eq!(
            got,
            vec![PathBuf::from("/w/a.rs"), PathBuf::from("/w/b.rs")]
        );
    }

    #[test]
    fn reattach_respects_file_and_token_limits() {
        let d = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for i in 0..8 {
            let p = d.path().join(format!("f{i}.txt"));
            std::fs::write(&p, format!("content {i}")).unwrap();
            paths.push(p);
        }
        paths.push(d.path().join("missing.txt"));
        let text = render_reattached_files(&paths);
        assert!(text.contains("content 4") && !text.contains("content 5"));
        let big = d.path().join("big.txt");
        std::fs::write(&big, "word ".repeat(400_000)).unwrap();
        let text = render_reattached_files(&[big]);
        assert!(approx_token_count(&text) <= max_reattached_tokens() + 200);
    }
}
