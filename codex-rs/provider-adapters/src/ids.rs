//! Tool-call ids on the wire (ported from OpenCodex `createToolCallIdAllocator` /
//! `tool-call-id-remint.ts`).
//!
//! Providers accept `^[a-zA-Z0-9_-]{1,64}$`. A call and its result must resolve to the same wire
//! id, and two different raw ids must never collapse into one. Conforming ids are claimed first
//! so a rewritten id cannot squat on an id another call legitimately owns.

use std::collections::HashMap;
use std::collections::HashSet;

pub fn is_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[derive(Debug, Default)]
pub struct IdAllocator {
    reserved: HashSet<String>,
    assigned: HashMap<String, String>,
    next: u32,
}

impl IdAllocator {
    /// Claim every conforming id up front.
    pub fn reserve<'a>(&mut self, raw_ids: impl IntoIterator<Item = &'a str>) {
        for raw in raw_ids {
            if is_valid(raw) {
                self.reserved.insert(raw.to_string());
                self.assigned
                    .entry(raw.to_string())
                    .or_insert_with(|| raw.to_string());
            }
        }
    }

    /// The wire id for a raw call id, minting one when the raw id cannot be used.
    pub fn allocate(&mut self, raw: &str) -> String {
        if let Some(existing) = self.assigned.get(raw) {
            return existing.clone();
        }
        let minted = loop {
            self.next += 1;
            let candidate = format!("call_{}", self.next);
            if !self.reserved.contains(&candidate) {
                break candidate;
            }
        };
        self.reserved.insert(minted.clone());
        self.assigned.insert(raw.to_string(), minted.clone());
        minted
    }

    /// The id a call already got; never mints.
    pub fn lookup(&self, raw: &str) -> Option<String> {
        self.assigned.get(raw).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn valid_ids_pass_and_bad_ones_are_minted_consistently() {
        let mut ids = IdAllocator::default();
        ids.reserve(["call_1", "good-id", "bad id", "x.y"]);
        assert_eq!(ids.allocate("good-id"), "good-id");
        // `call_1` is taken by a real call, so the minted id skips it.
        let minted = ids.allocate("bad id");
        assert_ne!(minted, "call_1");
        assert!(is_valid(&minted));
        assert_eq!(ids.allocate("bad id"), minted);
        assert_eq!(ids.lookup("bad id"), Some(minted));
        assert_eq!(ids.lookup("never seen"), None);
        assert_ne!(ids.allocate("x.y"), ids.allocate("bad id"));
    }
}
