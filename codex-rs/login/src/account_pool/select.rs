//! Pure account selection: no I/O, no clocks, so every rule is testable.

use super::types::AccountKind;
use super::types::AccountPolicy;
use super::types::AccountUsage;
use super::types::ApiKeyOrder;
use super::types::SelectionStrategy;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: String,
    pub kind: AccountKind,
    pub enabled: bool,
    pub cooldown_until: Option<i64>,
    pub priority: i32,
    pub usage: Option<AccountUsage>,
    /// Typical percentage points one turn adds; used to foresee crossing the threshold.
    pub pace: f64,
}

impl Candidate {
    pub fn usable(&self, now: i64) -> bool {
        self.enabled && self.cooldown_until.is_none_or(|until| until <= now)
    }

    /// Whether the next turn is expected to push this account over the threshold.
    pub fn over_threshold(&self, policy: &AccountPolicy, now: i64) -> bool {
        if self.kind != AccountKind::Chatgpt {
            return false;
        }
        let Some(used) = self
            .usage
            .as_ref()
            .and_then(|usage| usage.effective_percent(policy.window, now))
        else {
            return false;
        };
        if used >= 100.0 {
            return true;
        }
        policy.auto_switch && used + self.pace.max(0.0) >= policy.threshold_percent
    }

    fn used(&self, policy: &AccountPolicy, now: i64) -> f64 {
        self.usage
            .as_ref()
            .and_then(|usage| usage.effective_percent(policy.window, now))
            .unwrap_or(0.0)
    }
}

pub struct SelectInput<'a> {
    pub candidates: &'a [Candidate],
    pub policy: &'a AccountPolicy,
    pub now: i64,
    pub current: Option<&'a str>,
    /// A manual choice; honored while it is usable.
    pub pinned: Option<&'a str>,
    /// Round robin moves on from this id.
    pub rr_after: Option<&'a str>,
    /// Round robin advances only when this is set (a new turn); a continuing turn stays put.
    pub advance: bool,
}

pub fn select(input: &SelectInput<'_>) -> Option<String> {
    let SelectInput {
        candidates,
        policy,
        now,
        current,
        pinned,
        rr_after,
        advance,
    } = *input;

    if let Some(pin) = pinned
        && candidates.iter().any(|c| c.id == pin && c.usable(now))
    {
        return Some(pin.to_string());
    }

    let usable: Vec<&Candidate> = candidates.iter().filter(|c| c.usable(now)).collect();
    let (ok, over): (Vec<&Candidate>, Vec<&Candidate>) =
        usable.into_iter().partition(|c| !c.over_threshold(policy, now));
    let chatgpt_ok: Vec<&Candidate> = ok
        .iter()
        .copied()
        .filter(|c| c.kind == AccountKind::Chatgpt)
        .collect();
    let keys_ok: Vec<&Candidate> = ok
        .iter()
        .copied()
        .filter(|c| c.kind == AccountKind::ApiKey)
        .collect();

    // The last tier holds accounts already over the threshold: a last resort, never a resting place.
    let tiers: Vec<Vec<&Candidate>> = match policy.api_key_order {
        ApiKeyOrder::AfterChatgpt => vec![chatgpt_ok, keys_ok, over],
        ApiKeyOrder::BeforeChatgpt => vec![keys_ok, chatgpt_ok, over],
        ApiKeyOrder::Mixed => vec![ok, over],
    };
    let last = tiers.len() - 1;
    let (tier_index, tier) = tiers
        .into_iter()
        .enumerate()
        .find(|(_, tier)| !tier.is_empty())?;

    let stay_put = tier_index != last && (policy.selection != SelectionStrategy::RoundRobin || !advance);
    if stay_put
        && let Some(current) = current
        && tier.iter().any(|c| c.id == current)
    {
        return Some(current.to_string());
    }

    let mut ranked = tier;
    match policy.selection {
        SelectionStrategy::Quota => ranked.sort_by(|a, b| {
            a.used(policy, now)
                .total_cmp(&b.used(policy, now))
                .then(b.priority.cmp(&a.priority))
                .then(a.id.cmp(&b.id))
        }),
        SelectionStrategy::FillFirst | SelectionStrategy::RoundRobin => {
            ranked.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
        }
    }
    if policy.selection == SelectionStrategy::RoundRobin
        && let Some(after) = rr_after.or(current)
        && let Some(index) = ranked.iter().position(|c| c.id == after)
    {
        return Some(ranked[(index + 1) % ranked.len()].id.clone());
    }
    ranked.first().map(|c| c.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_pool::types::UsageWindow;
    use crate::account_pool::types::WindowKind;
    use pretty_assertions::assert_eq;

    fn usage(five: f64, week: f64) -> AccountUsage {
        AccountUsage {
            five_hour: Some(UsageWindow {
                used_percent: five,
                window_minutes: Some(300),
                resets_at: Some(10_000),
            }),
            weekly: Some(UsageWindow {
                used_percent: week,
                window_minutes: Some(10_080),
                resets_at: Some(20_000),
            }),
            updated_at: Some(0),
        }
    }

    fn chatgpt(id: &str, priority: i32, five: f64, week: f64) -> Candidate {
        Candidate {
            id: id.to_string(),
            kind: AccountKind::Chatgpt,
            enabled: true,
            cooldown_until: None,
            priority,
            usage: Some(usage(five, week)),
            pace: 0.0,
        }
    }

    fn key(id: &str, priority: i32) -> Candidate {
        Candidate {
            id: id.to_string(),
            kind: AccountKind::ApiKey,
            enabled: true,
            cooldown_until: None,
            priority,
            usage: None,
            pace: 0.0,
        }
    }

    fn pick(
        candidates: &[Candidate],
        policy: &AccountPolicy,
        current: Option<&str>,
        advance: bool,
    ) -> Option<String> {
        select(&SelectInput {
            candidates,
            policy,
            now: 100,
            current,
            pinned: None,
            rr_after: None,
            advance,
        })
    }

    #[test]
    fn quota_prefers_lowest_usage_and_sticks_to_current() {
        let policy = AccountPolicy::default();
        let accounts = [chatgpt("a", 0, 50.0, 10.0), chatgpt("b", 0, 20.0, 10.0)];
        assert_eq!(pick(&accounts, &policy, None, true), Some("b".into()));
        // Staying on the current account keeps the prompt cache warm.
        assert_eq!(pick(&accounts, &policy, Some("a"), true), Some("a".into()));
    }

    #[test]
    fn crossing_the_threshold_moves_on() {
        let policy = AccountPolicy::default();
        let accounts = [chatgpt("a", 0, 91.0, 10.0), chatgpt("b", 0, 60.0, 10.0)];
        assert_eq!(pick(&accounts, &policy, Some("a"), false), Some("b".into()));
    }

    #[test]
    fn pace_foresees_the_crossing() {
        let policy = AccountPolicy::default();
        let mut a = chatgpt("a", 0, 85.0, 10.0);
        a.pace = 6.0;
        let accounts = [a, chatgpt("b", 0, 60.0, 10.0)];
        assert_eq!(pick(&accounts, &policy, Some("a"), false), Some("b".into()));
    }

    #[test]
    fn weekly_window_counts_with_both_but_not_with_five_hour_only() {
        let mut policy = AccountPolicy::default();
        let accounts = [chatgpt("a", 0, 10.0, 95.0), chatgpt("b", 0, 40.0, 40.0)];
        assert_eq!(pick(&accounts, &policy, Some("a"), false), Some("b".into()));
        policy.window = WindowKind::FiveHour;
        assert_eq!(pick(&accounts, &policy, Some("a"), false), Some("a".into()));
    }

    #[test]
    fn a_reset_window_counts_as_unused() {
        let policy = AccountPolicy::default();
        let mut a = chatgpt("a", 0, 99.0, 99.0);
        if let Some(usage) = a.usage.as_mut() {
            usage.five_hour.as_mut().unwrap().resets_at = Some(50);
            usage.weekly.as_mut().unwrap().resets_at = Some(60);
        }
        assert_eq!(pick(&[a], &policy, None, true), Some("a".into()));
    }

    #[test]
    fn cooldown_and_disabled_are_skipped() {
        let policy = AccountPolicy::default();
        let mut a = chatgpt("a", 9, 0.0, 0.0);
        a.cooldown_until = Some(500);
        let mut b = chatgpt("b", 9, 0.0, 0.0);
        b.enabled = false;
        let accounts = [a, b, chatgpt("c", 0, 70.0, 0.0)];
        assert_eq!(pick(&accounts, &policy, None, true), Some("c".into()));
    }

    #[test]
    fn api_keys_come_after_chatgpt_by_default() {
        let policy = AccountPolicy::default();
        let accounts = [key("k", 100), chatgpt("a", 0, 30.0, 0.0)];
        assert_eq!(pick(&accounts, &policy, None, true), Some("a".into()));
        let accounts = [key("k", 100), chatgpt("a", 0, 95.0, 0.0)];
        assert_eq!(pick(&accounts, &policy, Some("a"), false), Some("k".into()));
    }

    #[test]
    fn api_key_order_can_be_reversed_or_mixed() {
        let mut policy = AccountPolicy::default();
        let accounts = [key("k", 0), chatgpt("a", 0, 30.0, 0.0)];
        policy.api_key_order = ApiKeyOrder::BeforeChatgpt;
        assert_eq!(pick(&accounts, &policy, None, true), Some("k".into()));
        policy.api_key_order = ApiKeyOrder::Mixed;
        // Keys have no usage, so quota ranks them first.
        assert_eq!(pick(&accounts, &policy, None, true), Some("k".into()));
    }

    #[test]
    fn when_everything_is_over_the_threshold_the_least_used_is_still_used() {
        let policy = AccountPolicy::default();
        let accounts = [chatgpt("a", 0, 97.0, 0.0), chatgpt("b", 0, 92.0, 0.0)];
        assert_eq!(pick(&accounts, &policy, Some("a"), false), Some("b".into()));
    }

    #[test]
    fn fill_first_uses_priority_order() {
        let mut policy = AccountPolicy::default();
        policy.selection = SelectionStrategy::FillFirst;
        let accounts = [chatgpt("a", 1, 10.0, 0.0), chatgpt("b", 5, 80.0, 0.0)];
        assert_eq!(pick(&accounts, &policy, None, true), Some("b".into()));
    }

    #[test]
    fn round_robin_advances_on_a_new_turn_only() {
        let mut policy = AccountPolicy::default();
        policy.selection = SelectionStrategy::RoundRobin;
        let accounts = [
            chatgpt("a", 0, 10.0, 0.0),
            chatgpt("b", 0, 10.0, 0.0),
            chatgpt("c", 0, 10.0, 0.0),
        ];
        assert_eq!(pick(&accounts, &policy, Some("a"), true), Some("b".into()));
        assert_eq!(pick(&accounts, &policy, Some("c"), true), Some("a".into()));
        assert_eq!(pick(&accounts, &policy, Some("a"), false), Some("a".into()));
    }

    #[test]
    fn a_pin_wins_until_it_is_unusable() {
        let policy = AccountPolicy::default();
        let mut pinned = chatgpt("a", 0, 99.0, 0.0);
        let others = chatgpt("b", 0, 1.0, 0.0);
        let chosen = select(&SelectInput {
            candidates: &[pinned.clone(), others.clone()],
            policy: &policy,
            now: 100,
            current: Some("b"),
            pinned: Some("a"),
            rr_after: None,
            advance: true,
        });
        assert_eq!(chosen, Some("a".into()));
        pinned.cooldown_until = Some(1_000);
        let chosen = select(&SelectInput {
            candidates: &[pinned, others],
            policy: &policy,
            now: 100,
            current: Some("a"),
            pinned: Some("a"),
            rr_after: None,
            advance: true,
        });
        assert_eq!(chosen, Some("b".into()));
    }

    #[test]
    fn nothing_usable_selects_nothing() {
        let policy = AccountPolicy::default();
        let mut a = chatgpt("a", 0, 0.0, 0.0);
        a.enabled = false;
        assert_eq!(pick(&[a], &policy, None, true), None);
    }

    #[test]
    fn auto_switch_off_ignores_the_threshold_but_not_a_full_window() {
        let mut policy = AccountPolicy::default();
        policy.auto_switch = false;
        let accounts = [chatgpt("a", 0, 95.0, 0.0), chatgpt("b", 0, 10.0, 0.0)];
        assert_eq!(pick(&accounts, &policy, Some("a"), false), Some("a".into()));
        let accounts = [chatgpt("a", 0, 100.0, 0.0), chatgpt("b", 0, 10.0, 0.0)];
        assert_eq!(pick(&accounts, &policy, Some("a"), false), Some("b".into()));
    }
}
