//! Wire translation for providers that do not speak the Responses API.
//!
//! Pure functions and small state machines, no I/O: `history` and `tool_names` read the Responses
//! request, `chat` (and later `messages`) build the provider body and parse the provider stream
//! into [`ir::AdapterEvent`]s, and `events::Assembler` turns those back into Responses items.
//! The HTTP client lives in `codex-api` and uses this crate.
//!
//! Ported from OpenCodex (MIT); see `NOTICE`.

pub mod chat;
pub mod envelope;
pub mod events;
pub mod history;
pub mod ids;
pub mod ir;
pub mod tool_names;
