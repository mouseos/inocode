//! `team.auto_select`: look at a user request, and when it contains several independent
//! pieces of work, estimate the speed-up and produce a recommendation for the main model.

use crate::estimate::Estimate;
use crate::estimate::EstimateConfig;
use crate::estimate::Task;
use crate::estimate::estimate;
use crate::estimate::recommendation_text;

/// Default expected duration of one detected task, seconds.
pub const DEFAULT_TASK_SECONDS: f64 = 600.0;

fn list_item(line: &str) -> Option<&str> {
    let t = line.trim_start();
    for p in ["- ", "* ", "・", "• "] {
        if let Some(rest) = t.strip_prefix(p) {
            return Some(rest.trim());
        }
    }
    // "1. foo", "2) foo", "①foo"
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let rest = &t[digits.len()..];
        for p in [". ", ") ", "、", "."] {
            if let Some(r) = rest.strip_prefix(p) {
                return Some(r.trim());
            }
        }
    }
    let mut chars = t.chars();
    if let Some(c) = chars.next()
        && ('①'..='⑳').contains(&c)
    {
        return Some(chars.as_str().trim());
    }
    None
}

/// Independent tasks named by the request: its list items. Write sets are unknown at this
/// point, so none are claimed (the model decides the split and the files).
pub fn detect_tasks(user_text: &str) -> Vec<Task> {
    user_text
        .lines()
        .filter_map(list_item)
        .filter(|s| !s.is_empty())
        .map(|s| Task {
            name: s.chars().take(60).collect(),
            write_paths: Vec::new(),
            est_seconds: DEFAULT_TASK_SECONDS,
        })
        .collect()
}

/// `(estimate, recommendation text)` when the request holds two or more independent tasks.
pub fn recommend_for_input(
    user_text: &str,
    cfg: &EstimateConfig,
) -> Option<(Estimate, String)> {
    let tasks = detect_tasks(user_text);
    if tasks.len() < 2 {
        return None;
    }
    let e = estimate(&tasks, cfg);
    let text = recommendation_text(&e);
    Some((e, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::Decision;

    #[test]
    fn no_list_no_recommendation() {
        assert!(recommend_for_input("fix the bug", &EstimateConfig::default()).is_none());
    }

    #[test]
    fn list_of_tasks_recommends_multi() {
        let text = "Do these:\n1. add the API\n2. write the CLI\n- update docs";
        let (e, rec) = recommend_for_input(text, &EstimateConfig::default()).unwrap();
        assert_eq!(e.decision, Decision::Multi);
        assert!(rec.contains("recommended"));
    }
}
