//! Carries a provider's opaque reasoning data through the `encrypted_content` slot of a
//! Responses reasoning item (ported from OpenCodex `responses/reasoning-envelope.ts`).
//!
//! Anthropic requires the previous assistant turn's `thinking` / `redacted_thinking` blocks to be
//! replayed verbatim, with their signatures, while extended thinking is on. Codex round-trips
//! whatever `encrypted_content` a reasoning item carries, so the signature rides inside
//! `inocode-r1:` + base64(JSON). Native OpenAI blobs have no prefix and are left alone.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde::Serialize;

pub const PREFIX: &str = "inocode-r1:";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Thinking-block signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    /// Raw `redacted_thinking` payloads, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub red: Vec<String>,
    /// Thinking text that was not shown but is covered by the signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txt: Option<String>,
}

impl Envelope {
    pub fn is_empty(&self) -> bool {
        self.sig.is_none() && self.red.is_empty() && self.txt.is_none()
    }

    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).unwrap_or_default();
        format!("{PREFIX}{}", STANDARD.encode(json))
    }

    /// `None` for anything that is not one of our envelopes (including native OpenAI blobs).
    pub fn decode(encrypted_content: &str) -> Option<Self> {
        let body = encrypted_content.strip_prefix(PREFIX)?;
        let bytes = STANDARD.decode(body).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn round_trips_and_ignores_foreign_blobs() {
        let envelope = Envelope {
            sig: Some("sig-123".to_string()),
            red: vec!["r1".to_string(), "r2".to_string()],
            txt: Some("hidden".to_string()),
        };
        assert_eq!(Envelope::decode(&envelope.encode()), Some(envelope));
        assert_eq!(Envelope::decode("gAAAAABopenai-blob"), None);
        assert_eq!(Envelope::decode("inocode-r1:!!!"), None);
    }
}
