//! The log of requests to models, and what it tells per model: tokens, how
//! much was read from the cache, how soon the answer starts and how fast it
//! comes, also by hour of the day. Design: docs/evidence/usage-log.md.

use std::collections::BTreeMap;
use std::time::Duration;

use rusqlite::params;
use serde::Serialize;

use crate::store::{store_error, SessionId, Store};
use crate::{ErrorInfo, RequestRecord};

/// Requests of one model, summed up.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelStats {
    pub provider: String,
    pub model: String,
    pub requests: u64,
    pub failed: u64,
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    /// The part of the input tokens read from the cache.
    pub cache_share: Option<f64>,
    /// The median time from sending to the first text, in milliseconds.
    pub first_output_ms: Option<u64>,
    /// The median speed of the answers, in output tokens per second.
    pub tokens_per_second: Option<f64>,
    /// The same by hour of the day, in the time zone the caller gave.
    pub by_hour: Vec<HourStats>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HourStats {
    pub hour: u8,
    pub requests: u64,
    pub first_output_ms: Option<u64>,
    pub tokens_per_second: Option<f64>,
}

/// One logged request as read back.
struct Row {
    at: i64,
    provider: String,
    model: String,
    input: Option<i64>,
    output: Option<i64>,
    cached: Option<i64>,
    first_output_ms: Option<i64>,
    duration_ms: i64,
    outcome: String,
}

impl Store {
    /// Logs the requests of a turn, all in one write.
    pub(crate) async fn log_requests(
        &self,
        session: SessionId,
        records: &[RequestRecord],
    ) -> Result<(), ErrorInfo> {
        if records.is_empty() {
            return Ok(());
        }
        let records = records.to_vec();
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            for record in &records {
                tx.execute(
                    "INSERT INTO requests(at, session_id, purpose, provider, model, input,
                                          output, cached, first_output_ms, duration_ms, outcome)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        record.at_ms as i64,
                        session.0,
                        record.purpose,
                        record.provider,
                        record.model,
                        record.usage.map(|usage| usage.input as i64),
                        record.usage.map(|usage| usage.output as i64),
                        record.usage.map(|usage| usage.cached as i64),
                        record.first_output_ms.map(|ms| ms as i64),
                        record.duration_ms as i64,
                        record.outcome,
                    ],
                )
                .map_err(store_error)?;
            }
            tx.commit().map_err(store_error)
        })
        .await
    }

    /// The requests since `since_ms` (milliseconds since the Unix epoch),
    /// per provider and model, the busiest first. Hours are counted in the
    /// time zone `utc_offset_minutes` ahead of UTC.
    pub async fn model_stats(
        &self,
        since_ms: u64,
        utc_offset_minutes: i64,
    ) -> Result<Vec<ModelStats>, ErrorInfo> {
        let rows = self
            .read(Duration::from_secs(30), move |db| {
                let mut statement = db
                    .prepare_cached(
                        "SELECT at, provider, model, input, output, cached, first_output_ms,
                                duration_ms, outcome
                         FROM requests WHERE at >= ?1 ORDER BY at",
                    )
                    .map_err(store_error)?;
                let rows = statement
                    .query_map([since_ms as i64], |row| {
                        Ok(Row {
                            at: row.get(0)?,
                            provider: row.get(1)?,
                            model: row.get(2)?,
                            input: row.get(3)?,
                            output: row.get(4)?,
                            cached: row.get(5)?,
                            first_output_ms: row.get(6)?,
                            duration_ms: row.get(7)?,
                            outcome: row.get(8)?,
                        })
                    })
                    .map_err(store_error)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(store_error)
            })
            .await?;
        Ok(summarize(rows, utc_offset_minutes))
    }
}

fn summarize(rows: Vec<Row>, utc_offset_minutes: i64) -> Vec<ModelStats> {
    let mut groups: BTreeMap<(String, String), Vec<Row>> = BTreeMap::new();
    for row in rows {
        groups
            .entry((row.provider.clone(), row.model.clone()))
            .or_default()
            .push(row);
    }
    let mut models: Vec<ModelStats> = groups
        .into_iter()
        .map(|((provider, model), rows)| {
            let sum = |value: fn(&Row) -> Option<i64>| -> u64 {
                rows.iter()
                    .filter_map(value)
                    .map(|value| value.max(0) as u64)
                    .sum()
            };
            let input = sum(|row| row.input);
            let cached = sum(|row| row.cached);
            let completed: Vec<&Row> = rows
                .iter()
                .filter(|row| row.outcome == "completed")
                .collect();
            let mut by_hour = Vec::new();
            for hour in 0..24u8 {
                let in_hour: Vec<&Row> = completed
                    .iter()
                    .copied()
                    .filter(|row| hour_of(row.at, utc_offset_minutes) == hour)
                    .collect();
                if in_hour.is_empty() {
                    continue;
                }
                by_hour.push(HourStats {
                    hour,
                    requests: in_hour.len() as u64,
                    first_output_ms: median_first_output(&in_hour),
                    tokens_per_second: median_speed(&in_hour),
                });
            }
            ModelStats {
                requests: rows.len() as u64,
                failed: rows.iter().filter(|row| row.outcome == "failed").count() as u64,
                input,
                output: sum(|row| row.output),
                cached,
                cache_share: (input > 0).then(|| cached as f64 / input as f64),
                first_output_ms: median_first_output(&completed),
                tokens_per_second: median_speed(&completed),
                by_hour,
                provider,
                model,
            }
        })
        .collect();
    models.sort_by(|a, b| b.requests.cmp(&a.requests));
    models
}

/// The hour of the day of `at_ms` in the time zone `offset` minutes ahead
/// of UTC.
fn hour_of(at_ms: i64, offset_minutes: i64) -> u8 {
    let minutes = at_ms.div_euclid(60_000) + offset_minutes;
    (minutes.div_euclid(60).rem_euclid(24)) as u8
}

/// Output tokens per second of one answer: over the time after its first
/// text when that is known, else over the whole request.
fn speed(row: &Row) -> Option<f64> {
    let output = row.output.filter(|output| *output > 0)? as f64;
    let generating = match row.first_output_ms {
        Some(first) if row.duration_ms > first => row.duration_ms - first,
        _ => row.duration_ms,
    };
    (generating > 0).then(|| output / (generating as f64 / 1000.0))
}

fn median_speed(rows: &[&Row]) -> Option<f64> {
    let mut speeds: Vec<f64> = rows.iter().filter_map(|row| speed(row)).collect();
    if speeds.is_empty() {
        return None;
    }
    speeds.sort_by(f64::total_cmp);
    Some(speeds[speeds.len() / 2])
}

fn median_first_output(rows: &[&Row]) -> Option<u64> {
    let mut times: Vec<u64> = rows
        .iter()
        .filter_map(|row| row.first_output_ms)
        .map(|ms| ms.max(0) as u64)
        .collect();
    if times.is_empty() {
        return None;
    }
    times.sort_unstable();
    Some(times[times.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TokenUsage;

    fn record(
        model: &str,
        at_ms: u64,
        output: u64,
        first: Option<u64>,
        duration: u64,
    ) -> RequestRecord {
        RequestRecord {
            at_ms,
            purpose: "turn",
            provider: "openai@example.com".into(),
            model: model.into(),
            usage: Some(TokenUsage {
                input: 1_000,
                output,
                cached: 600,
            }),
            first_output_ms: first,
            duration_ms: duration,
            outcome: "completed",
        }
    }

    #[tokio::test]
    async fn requests_are_logged_and_summed_up_per_model_and_hour() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path().join("inocode.db"))
            .await
            .expect("store");
        let session = store.start_session(None, "t").await.expect("session");
        // 2026-01-01 08:00 and 09:00 UTC.
        let eight = 1_767_254_400_000;
        let nine = eight + 3_600_000;
        let mut failed = record("slow", nine, 0, None, 5_000);
        failed.outcome = "failed";
        failed.usage = None;
        store
            .log_requests(
                session,
                &[
                    // 100 tokens in 1 s after the first text at 0.5 s.
                    record("fast", eight, 100, Some(500), 1_500),
                    record("fast", eight + 1, 300, Some(700), 2_200),
                    record("fast", nine, 100, None, 4_000),
                    record("slow", nine, 50, Some(2_000), 7_000),
                    failed,
                ],
            )
            .await
            .expect("log");

        let stats = store.model_stats(0, 0).await.expect("stats");
        assert_eq!(stats.len(), 2);
        let fast = &stats[0];
        assert_eq!(
            (fast.model.as_str(), fast.requests, fast.failed),
            ("fast", 3, 0)
        );
        assert_eq!((fast.input, fast.output, fast.cached), (3_000, 500, 1_800));
        assert_eq!(fast.cache_share, Some(0.6));
        assert_eq!(fast.first_output_ms, Some(700));
        // Speeds 100, 200 and 25 tokens/s: the median is 100.
        assert_eq!(fast.tokens_per_second, Some(100.0));
        assert_eq!(
            fast.by_hour
                .iter()
                .map(|hour| (hour.hour, hour.requests))
                .collect::<Vec<_>>(),
            [(8, 2), (9, 1)]
        );
        let slow = &stats[1];
        assert_eq!((slow.requests, slow.failed, slow.output), (2, 1, 50));
        assert_eq!(slow.tokens_per_second, Some(10.0));

        // In Japan, nine hours ahead.
        let japan = store.model_stats(0, 540).await.expect("stats");
        assert_eq!(japan[0].by_hour[0].hour, 17);
        // Only what is recent enough.
        assert!(store
            .model_stats(nine + 1, 0)
            .await
            .expect("stats")
            .is_empty());
    }

    #[test]
    fn hours_wrap_around_the_day() {
        assert_eq!(hour_of(0, -60), 23);
        assert_eq!(hour_of(23 * 3_600_000, 120), 1);
    }
}
