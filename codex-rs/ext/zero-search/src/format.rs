use std::collections::HashSet;

use zero_search::EngineOutcome;
use zero_search::SearchItem;

/// Largest tool result handed to the model.
pub(crate) const MAX_OUTPUT_BYTES: usize = 24 * 1024;

pub(crate) fn truncate_output(mut text: String) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    const SUFFIX: &str = "\n… [truncated]";
    text.truncate(text.floor_char_boundary(MAX_OUTPUT_BYTES - SUFFIX.len()));
    text.push_str(SUFFIX);
    text
}

/// Merge the engines' results round-robin (so one engine cannot crowd out the rest),
/// drop repeated URLs and keep at most `limit` entries.
pub(crate) fn merge_items(outcomes: &[EngineOutcome], limit: usize) -> Vec<&SearchItem> {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    let mut position = 0;
    while merged.len() < limit {
        let mut any = false;
        for outcome in outcomes.iter().filter(|outcome| outcome.ok) {
            let Some(item) = outcome.items.get(position) else {
                continue;
            };
            any = true;
            if merged.len() < limit && seen.insert(item.url.clone()) {
                merged.push(item);
            }
        }
        if !any {
            break;
        }
        position += 1;
    }
    merged
}

fn error_label(outcome: &EngineOutcome) -> String {
    let kind = outcome
        .error_kind
        .and_then(|kind| serde_json::to_value(kind).ok())
        .and_then(|value| value.as_str().map(str::to_string));
    match kind {
        Some(kind) => format!("{} ({kind})", outcome.engine.id()),
        None => outcome.engine.id().to_string(),
    }
}

/// Render web results for the model. The flag is false when no engine returned anything.
pub(crate) fn format_web_results(
    query: &str,
    outcomes: &[EngineOutcome],
    limit: usize,
) -> (String, bool) {
    let items = merge_items(outcomes, limit);
    let failed: Vec<String> = outcomes
        .iter()
        .filter(|outcome| !outcome.ok)
        .map(error_label)
        .collect();
    let mut text = format!("Web search results for {query:?}\n");
    if items.is_empty() {
        text.push_str("No results.\n");
    }
    for (index, item) in items.iter().enumerate() {
        text.push_str(&format!(
            "{}. {}\n   {}  [{}]\n",
            index + 1,
            item.title.trim(),
            item.url,
            item.engine.id()
        ));
        if let Some(snippet) = item.snippet.as_deref().map(str::trim)
            && !snippet.is_empty()
        {
            text.push_str(&format!("   {snippet}\n"));
        }
    }
    if !failed.is_empty() {
        text.push_str(&format!("Engines that failed: {}\n", failed.join(", ")));
    }
    (truncate_output(text), !items.is_empty())
}

#[cfg(test)]
#[path = "format_tests.rs"]
mod tests;
