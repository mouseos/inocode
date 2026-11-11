//! `inocode/metrics/*`: aggregates the append-only log written by `codex-inocode-metrics`.

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::internal_error;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeMetricsBucket;
use codex_app_server_protocol::InocodeMetricsCounters;
use codex_app_server_protocol::InocodeMetricsHistoryBucket;
use codex_app_server_protocol::InocodeMetricsHistoryParams;
use codex_app_server_protocol::InocodeMetricsHistoryResponse;
use codex_app_server_protocol::InocodeMetricsMarkFalsePositiveResponse;
use codex_app_server_protocol::InocodeMetricsSummaryResponse;
use codex_app_server_protocol::InocodeMetricsThread;
use codex_inocode_metrics::MetricCounters;
use codex_inocode_metrics::MetricEvent;
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
        stalls_detected: to_i64(c.stalls_detected),
    }
}

/// RFC 3339 timestamp to epoch milliseconds; anything unparsable means "since the beginning".
fn since_ms(since: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(since)
        .ok()
        .and_then(|t| u64::try_from(t.timestamp_millis()).ok())
        .unwrap_or(0)
}

pub(crate) fn summary_since(
    store: &MetricsStore,
    since: &str,
    thread_id: Option<&str>,
) -> InocodeMetricsSummaryResponse {
    let summary = codex_inocode_metrics::summarize(&store.read_thread(since_ms(since), thread_id));
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

pub(crate) fn history_since(
    store: &MetricsStore,
    params: &InocodeMetricsHistoryParams,
) -> InocodeMetricsHistoryResponse {
    let width = match params.bucket {
        InocodeMetricsBucket::Hour => codex_inocode_metrics::HOUR_MS,
        InocodeMetricsBucket::Day => codex_inocode_metrics::DAY_MS,
    };
    InocodeMetricsHistoryResponse {
        buckets: store
            .history(since_ms(&params.since), width, params.thread_id.as_deref())
            .iter()
            .map(|b| InocodeMetricsHistoryBucket {
                start_ms: i64::try_from(b.start_ms).unwrap_or(i64::MAX),
                counters: counters(&b.counters),
            })
            .collect(),
    }
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeMetricsSummary { params, .. } => Ok(Some(
            summary_since(
                &MetricsStore::from_env(),
                &params.since,
                params.thread_id.as_deref(),
            )
            .into(),
        )),
        ClientRequest::InocodeMetricsHistory { params, .. } => {
            Ok(Some(history_since(&MetricsStore::from_env(), &params).into()))
        }
        ClientRequest::InocodeMetricsMarkFalsePositive { params, .. } => {
            MetricsStore::from_env()
                .record(
                    &params.thread_id,
                    MetricEvent::FalsePositive {
                        source: params.source,
                    },
                )
                .map_err(|err| internal_error(format!("failed to record false positive: {err}")))?;
            Ok(Some(InocodeMetricsMarkFalsePositiveResponse::default().into()))
        }
        other => Err(unknown_method(other.method_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let all = summary_since(&store, "not a date", None);
        assert_eq!(all.counters.wait_agent_calls, 2);
        let recent = summary_since(&store, "2026-11-11T00:00:00Z", None);
        assert_eq!(recent.counters.wait_agent_calls, 1);
        assert_eq!(recent.counters.tokens_in, 7);
        let threads = recent.per_thread.expect("per thread");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, "t1");
    }

    #[test]
    fn summary_includes_stalls_and_filters_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::at(dir.path());
        let stall = || MetricEvent::StallDetected {
            trigger: "same_failure".to_string(),
        };
        store.record_at(10, "a", stall()).expect("record");
        store.record_at(11, "b", stall()).expect("record");
        store
            .record_at(12, "b", MetricEvent::FalsePositive { source: "guard".into() })
            .expect("record");
        assert_eq!(summary_since(&store, "x", None).counters.stalls_detected, 2);
        let only_b = summary_since(&store, "x", Some("b"));
        assert_eq!(only_b.counters.stalls_detected, 1);
        assert_eq!(only_b.counters.false_positives, 1);
    }

    #[test]
    fn history_groups_by_hour() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::at(dir.path());
        store.record_at(1_000, "a", MetricEvent::WaitAgentCall).expect("record");
        store
            .record_at(3_600_000 + 1, "a", MetricEvent::WaitAgentCall)
            .expect("record");
        let res = history_since(
            &store,
            &InocodeMetricsHistoryParams {
                since: "x".into(),
                bucket: InocodeMetricsBucket::Hour,
                thread_id: None,
            },
        );
        assert_eq!(res.buckets.len(), 2);
        assert_eq!(res.buckets[1].start_ms, 3_600_000);
    }
}
