//! Append-only measurement log for InoCode agent threads.
//!
//! Events are stored one JSON object per line under `<INOCODE_HOME>/metrics/events.jsonl`
//! and aggregated on demand into the shape returned by `inocode/metrics/summary`.

use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// One measured fact about a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MetricEvent {
    /// Wall-clock time of one finished turn.
    TurnWallClock { ms: u64 },
    /// Token usage reported for one model response.
    Tokens { input: u64, output: u64 },
    /// One `wait_agent` call.
    WaitAgentCall,
    /// The agent asked the user (or a peer) a clarifying question.
    Clarification,
    /// A guard, verifier or trigger fired and turned out to be wrong.
    FalsePositive { source: String },
    /// A stall trigger fired (advisor). `trigger` names which one.
    StallDetected { trigger: String },
}

/// A stored line: event plus thread and timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricRecord {
    pub ts_ms: u64,
    pub thread_id: String,
    #[serde(flatten)]
    pub event: MetricEvent,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricCounters {
    pub wall_clock_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub wait_agent_calls: u64,
    pub clarifications: u64,
    pub false_positives: u64,
    pub stalls_detected: u64,
}

impl MetricCounters {
    fn apply(&mut self, event: &MetricEvent) {
        match event {
            MetricEvent::TurnWallClock { ms } => self.wall_clock_ms += ms,
            MetricEvent::Tokens { input, output } => {
                self.tokens_in += input;
                self.tokens_out += output;
            }
            MetricEvent::WaitAgentCall => self.wait_agent_calls += 1,
            MetricEvent::Clarification => self.clarifications += 1,
            MetricEvent::FalsePositive { .. } => self.false_positives += 1,
            MetricEvent::StallDetected { .. } => self.stalls_detected += 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadMetrics {
    pub thread_id: String,
    #[serde(flatten)]
    pub counters: MetricCounters,
}

/// Response of `inocode/metrics/summary`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsSummary {
    pub counters: MetricCounters,
    pub per_thread: Vec<ThreadMetrics>,
}

/// Resolves `INOCODE_HOME`, falling back to `~/.inocode`.
pub fn inocode_home() -> PathBuf {
    if let Some(home) = std::env::var_os("INOCODE_HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".inocode")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// File-backed store. Cheap to construct; appends are serialized in-process.
pub struct MetricsStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl MetricsStore {
    pub fn at(inocode_home: &Path) -> Self {
        Self {
            path: inocode_home.join("metrics").join("events.jsonl"),
            write_lock: Mutex::new(()),
        }
    }

    pub fn from_env() -> Self {
        Self::at(&inocode_home())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, thread_id: &str, event: MetricEvent) -> std::io::Result<()> {
        self.record_at(now_ms(), thread_id, event)
    }

    pub fn record_at(
        &self,
        ts_ms: u64,
        thread_id: &str,
        event: MetricEvent,
    ) -> std::io::Result<()> {
        let record = MetricRecord {
            ts_ms,
            thread_id: thread_id.to_string(),
            event,
        };
        let mut line = serde_json::to_string(&record).map_err(std::io::Error::other)?;
        line.push('\n');
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())
    }

    /// All records at or after `since_ms` (0 = everything). Unparseable lines are skipped.
    pub fn read(&self, since_ms: u64) -> Vec<MetricRecord> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str::<MetricRecord>(line).ok())
            .filter(|r| r.ts_ms >= since_ms)
            .collect()
    }

    pub fn summary(&self, since_ms: u64) -> MetricsSummary {
        summarize(&self.read(since_ms))
    }

    /// Records at or after `since_ms`, optionally limited to one thread.
    pub fn read_thread(&self, since_ms: u64, thread_id: Option<&str>) -> Vec<MetricRecord> {
        let mut records = self.read(since_ms);
        if let Some(id) = thread_id {
            records.retain(|r| r.thread_id == id);
        }
        records
    }

    /// Time series of the counters, oldest first.
    pub fn history(
        &self,
        since_ms: u64,
        width_ms: u64,
        thread_id: Option<&str>,
    ) -> Vec<MetricsBucket> {
        bucketize(&self.read_thread(since_ms, thread_id), width_ms)
    }
}

/// One time bucket of the history: records whose `ts_ms` falls in `[start_ms, start_ms + width)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsBucket {
    pub start_ms: u64,
    pub counters: MetricCounters,
}

pub const HOUR_MS: u64 = 3_600_000;
pub const DAY_MS: u64 = 24 * HOUR_MS;

/// Groups records into fixed-width buckets (aligned to the epoch), oldest first.
/// Buckets without records are omitted.
pub fn bucketize(records: &[MetricRecord], width_ms: u64) -> Vec<MetricsBucket> {
    let width = width_ms.max(1);
    let mut buckets: BTreeMap<u64, MetricCounters> = BTreeMap::new();
    for r in records {
        buckets
            .entry(r.ts_ms - r.ts_ms % width)
            .or_default()
            .apply(&r.event);
    }
    buckets
        .into_iter()
        .map(|(start_ms, counters)| MetricsBucket { start_ms, counters })
        .collect()
}

pub fn summarize(records: &[MetricRecord]) -> MetricsSummary {
    let mut total = MetricCounters::default();
    let mut threads: BTreeMap<&str, MetricCounters> = BTreeMap::new();
    for r in records {
        total.apply(&r.event);
        threads.entry(&r.thread_id).or_default().apply(&r.event);
    }
    MetricsSummary {
        counters: total,
        per_thread: threads
            .into_iter()
            .map(|(id, counters)| ThreadMetrics {
                thread_id: id.to_string(),
                counters,
            })
            .collect(),
    }
}

/// Fire-and-forget record into the default store; errors are logged, never propagated.
pub fn record_global(thread_id: &str, event: MetricEvent) {
    if let Err(err) = MetricsStore::from_env().record(thread_id, event) {
        eprintln!("inocode-metrics: failed to record event: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn records_and_summarizes_per_thread() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::at(dir.path());
        store
            .record_at(10, "a", MetricEvent::TurnWallClock { ms: 500 })
            .unwrap();
        store
            .record_at(
                11,
                "a",
                MetricEvent::Tokens {
                    input: 100,
                    output: 20,
                },
            )
            .unwrap();
        store.record_at(12, "a", MetricEvent::WaitAgentCall).unwrap();
        store.record_at(13, "b", MetricEvent::WaitAgentCall).unwrap();
        store.record_at(14, "b", MetricEvent::Clarification).unwrap();
        store
            .record_at(
                15,
                "b",
                MetricEvent::FalsePositive {
                    source: "guard".into(),
                },
            )
            .unwrap();
        store
            .record_at(
                16,
                "b",
                MetricEvent::StallDetected {
                    trigger: "repeat".into(),
                },
            )
            .unwrap();

        let s = store.summary(0);
        assert_eq!(s.counters.wall_clock_ms, 500);
        assert_eq!(s.counters.tokens_in, 100);
        assert_eq!(s.counters.tokens_out, 20);
        assert_eq!(s.counters.wait_agent_calls, 2);
        assert_eq!(s.counters.clarifications, 1);
        assert_eq!(s.counters.false_positives, 1);
        assert_eq!(s.counters.stalls_detected, 1);
        assert_eq!(s.per_thread.len(), 2);
        assert_eq!(s.per_thread[0].thread_id, "a");
        assert_eq!(s.per_thread[1].counters.wait_agent_calls, 1);

        let since = store.summary(13);
        assert_eq!(since.counters.wait_agent_calls, 1);
        assert_eq!(since.counters.wall_clock_ms, 0);
    }

    #[test]
    fn history_buckets_by_width_and_filters_thread() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::at(dir.path());
        store.record_at(HOUR_MS + 5, "a", MetricEvent::WaitAgentCall).unwrap();
        store.record_at(HOUR_MS + 9, "b", MetricEvent::WaitAgentCall).unwrap();
        store
            .record_at(
                3 * HOUR_MS,
                "a",
                MetricEvent::StallDetected {
                    trigger: "same_failure".into(),
                },
            )
            .unwrap();
        let all = store.history(0, HOUR_MS, None);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].start_ms, HOUR_MS);
        assert_eq!(all[0].counters.wait_agent_calls, 2);
        assert_eq!(all[1].start_ms, 3 * HOUR_MS);
        assert_eq!(all[1].counters.stalls_detected, 1);
        let only_a = store.history(0, DAY_MS, Some("a"));
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].counters.wait_agent_calls, 1);
    }

    #[test]
    fn summary_json_uses_camel_case() {
        let json = serde_json::to_value(MetricsSummary::default()).unwrap();
        assert_eq!(json["counters"]["waitAgentCalls"], 0);
        assert!(json["perThread"].is_array());
    }

    #[test]
    fn missing_file_gives_empty_summary() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            MetricsStore::at(dir.path()).summary(0),
            MetricsSummary::default()
        );
    }
}
