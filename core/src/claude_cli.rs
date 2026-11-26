//! The version inocode gives as Claude Code's when it reads the Claude
//! account's usage and connectors: the one npm currently hands out on the
//! channel the settings choose (docs/evidence/claude-cli-version.md).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// npm's channels of `@anthropic-ai/claude-code`: a small JSON object of
/// channel name to version.
const DIST_TAGS: &str = "https://registry.npmjs.org/-/package/@anthropic-ai/claude-code/dist-tags";
/// Given when npm has not answered yet in this process.
const FALLBACK: &str = "2.1.321";
/// How long a read of the channels is used before it is read again.
const KEEP: Duration = Duration::from_secs(24 * 60 * 60);
/// How long after a failed read the next one is tried.
const RETRY: Duration = Duration::from_secs(60 * 60);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The channels the settings may choose (`server.claude_cli_channel`).
pub const CHANNELS: [&str; 2] = ["latest", "stable"];
/// The channel when the settings choose none.
pub const DEFAULT_CHANNEL: &str = "latest";

struct Read {
    /// The channels of the last read that worked; `None` before one did.
    tags: Option<HashMap<String, String>>,
    /// When the last read was tried, and whether it worked.
    tried: Instant,
    worked: bool,
}

static LAST: Mutex<Option<Read>> = Mutex::new(None);

/// The `User-Agent` Claude Code sends, with the version of the channel the
/// settings beside the credential store `store` choose.
pub(crate) async fn user_agent(store: &Path) -> String {
    let channel = channel(store);
    let tags = tags().await;
    format!(
        "claude-cli/{} (external, cli)",
        version(tags.as_ref(), &channel)
    )
}

fn channel(store: &Path) -> String {
    crate::settings::read_blocking(&store.with_file_name("settings.json"))
        .and_then(|value| crate::settings::Settings::from_value(&value))
        .ok()
        .and_then(|settings| settings.server.claude_cli_channel)
        .unwrap_or_else(|| DEFAULT_CHANNEL.to_string())
}

/// The channel's version from `tags`; [`FALLBACK`] when there is none or it
/// is not made of numbers and dots, since it goes into a header.
fn version(tags: Option<&HashMap<String, String>>, channel: &str) -> String {
    tags.and_then(|tags| tags.get(channel))
        .filter(|version| {
            !version.is_empty()
                && version
                    .split('.')
                    .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        })
        .cloned()
        .unwrap_or_else(|| FALLBACK.to_string())
}

/// The channels, read from npm again once the last read is too old.
async fn tags() -> Option<HashMap<String, String>> {
    let kept = {
        let last = LAST.lock().expect("claude-cli lock");
        match last.as_ref() {
            Some(read) => {
                let wait = if read.worked { KEEP } else { RETRY };
                (read.tried.elapsed() < wait).then(|| read.tags.clone())
            }
            None => None,
        }
    };
    if let Some(tags) = kept {
        return tags;
    }
    let fresh = fetch().await;
    let mut last = LAST.lock().expect("claude-cli lock");
    let previous = last.take().and_then(|read| read.tags);
    let read = match fresh {
        Ok(tags) => Read {
            tags: Some(tags),
            tried: Instant::now(),
            worked: true,
        },
        Err(error) => {
            eprintln!("Claude Code's version could not be read from npm: {error}");
            Read {
                tags: previous,
                tried: Instant::now(),
                worked: false,
            }
        }
    };
    let tags = read.tags.clone();
    *last = Some(read);
    tags
}

async fn fetch() -> Result<HashMap<String, String>, String> {
    let response = reqwest::Client::new()
        .get(DIST_TAGS)
        .timeout(READ_TIMEOUT)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("status {}", response.status()));
    }
    response
        .json::<HashMap<String, String>>()
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chosen_channel_gives_the_version_unless_it_is_missing_or_odd() {
        let tags: HashMap<String, String> = [
            ("latest", "2.1.332"),
            ("stable", "2.1.326"),
            ("next", "2.1.332; evil"),
        ]
        .into_iter()
        .map(|(channel, version)| (channel.to_string(), version.to_string()))
        .collect();
        assert_eq!(version(Some(&tags), "latest"), "2.1.332");
        assert_eq!(version(Some(&tags), "stable"), "2.1.326");
        assert_eq!(version(Some(&tags), "next"), FALLBACK);
        assert_eq!(version(Some(&tags), "beta"), FALLBACK);
        assert_eq!(version(None, "latest"), FALLBACK);
    }

    #[test]
    fn the_settings_choose_the_channel() {
        let dir = tempfile::tempdir().expect("dir");
        let store = dir.path().join("auth.json");
        assert_eq!(channel(&store), "latest");
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"server": {"claude_cli_channel": "stable"}}"#,
        )
        .expect("settings");
        assert_eq!(channel(&store), "stable");
    }
}
