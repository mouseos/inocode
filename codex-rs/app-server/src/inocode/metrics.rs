//! `inocode/metrics/*`: aggregates the append-only log written by `codex-inocode-metrics`.

use super::InocodeResult;
use super::unknown_method;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeMetricsCounters;
use codex_app_server_protocol::InocodeMetricsSummaryResponse;
use codex_app_server_protocol::InocodeMetricsThread;
use codex_inocode_metrics::MetricCounters;
use codex_inocode_metrics::MetricsStore;

fn counters(c: &MetricCounters) -> InocodeMetricsCounters {
    let to_i64 = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    InocodeMetricsCounters {
        wall_clock_ms: to_i64(c.wall_clock_ms),
        tokens_in: to_i64(c.tokens_in),
        tokens_out: to_i64(c.tokens_out),
        wait_agent_calls: to_i64(c.wait_agent_calls),
        clarifications: to_i64(c.clarifications),
        false_positives: to_i64(c.false_positives),
    }
}

/// RFC 3339 timestamp to epoch milliseconds; anything unparsable means "since the beginning".
fn since_ms(since: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(since)
        .ok()
        .and_then(|t| u64::try_from(t.timestamp_millis()).ok())
        .unwrap_or(0)
}

pub(crate) fn summary_since(store: &MetricsStore, since: &str) -> InocodeMetricsSummaryResponse {
    let summary = store.summary(since_ms(since));
    InocodeMetricsSummaryResponse {
        counters: counters(&summary.counters),
        per_thread: Some(
            summary
                .per_thread
                .iter()
                .map(|t| InocodeMetricsThread {
                    thread_id: t.thread_id.clone(),
                    counters: counters(&t.counters),
                })
                .collect(),
        ),
    }
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeMetricsSummary { params, .. } => Ok(Some(
            summary_since(&MetricsStore::from_env(), &params.since).into(),
        )),
        other => Err(unknown_method(other.method_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_inocode_metrics::MetricEvent;

    #[test]
    fn summary_reads_the_log_and_filters_by_since() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::at(dir.path());
        // 2026-11-11T00:00:00Z = 1_794_355_200_000 ms
        store
            .record_at(1_000, "old", MetricEvent::WaitAgentCall)
            .expect("record");
        store
            .record_at(1_794_355_300_000, "t1", MetricEvent::WaitAgentCall)
            .expect("record");
        store
            .record_at(
                1_794_355_300_001,
                "t1",
                MetricEvent::Tokens {
                    input: 7,
                    output: 3,
                },
            )
            .expect("record");
        let all = summary_since(&store, "not a date");
        assert_eq!(all.counters.wait_agent_calls, 2);
        let recent = summary_since(&store, "2026-11-11T00:00:00Z");
        assert_eq!(recent.counters.wait_agent_calls, 1);
        assert_eq!(recent.counters.tokens_in, 7);
        let threads = recent.per_thread.expect("per thread");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, "t1");
    }
}
