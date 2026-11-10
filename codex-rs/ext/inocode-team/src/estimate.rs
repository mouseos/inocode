use serde::Deserialize;
use serde::Serialize;

/// One independent piece of work.
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub name: String,
    /// Files / directories this task writes.
    pub write_paths: Vec<String>,
    /// Expected duration in seconds.
    pub est_seconds: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EstimateConfig {
    /// Multi is chosen when speedup >= this. Default 1.3.
    pub threshold: f64,
    /// Fixed cost of splitting the work, seconds.
    pub split_overhead_secs: f64,
    /// Fixed cost of integrating the results, seconds.
    pub integrate_overhead_secs: f64,
    /// Whether the boundary (contract) can be written up front.
    pub boundary_writable: bool,
    /// Extra serial time from dependencies that cannot be parallelised, seconds.
    pub serial_dependency_secs: f64,
}

impl Default for EstimateConfig {
    fn default() -> Self {
        Self {
            threshold: 1.3,
            split_overhead_secs: 60.0,
            integrate_overhead_secs: 60.0,
            boundary_writable: true,
            serial_dependency_secs: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Single,
    Multi,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Estimate {
    pub speedup: f64,
    /// Rough token cost relative to one agent (each extra agent re-reads context).
    pub cost_multiplier: f64,
    pub decision: Decision,
    pub reason: String,
}

impl Estimate {
    pub fn single(reason: impl Into<String>) -> Self {
        Self {
            speedup: 1.0,
            cost_multiplier: 1.0,
            decision: Decision::Single,
            reason: reason.into(),
        }
    }
}

fn paths_overlap(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_string();
    let (a, b) = (norm(a), norm(b));
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

fn any_overlap(tasks: &[Task]) -> bool {
    for (i, t) in tasks.iter().enumerate() {
        for u in &tasks[i + 1..] {
            if t.write_paths
                .iter()
                .any(|p| u.write_paths.iter().any(|q| paths_overlap(p, q)))
            {
                return true;
            }
        }
    }
    false
}

/// speedup = sequential / (longest assignee + split overhead + integrate overhead)
pub fn estimate(tasks: &[Task], cfg: &EstimateConfig) -> Estimate {
    if tasks.len() < 2 {
        return Estimate::single("fewer than two independent tasks");
    }
    if any_overlap(tasks) {
        return Estimate::single("write sets overlap between tasks; splitting would conflict");
    }
    if !cfg.boundary_writable {
        return Estimate::single("the boundary (contract) cannot be written up front");
    }
    let sequential: f64 =
        tasks.iter().map(|t| t.est_seconds).sum::<f64>() + cfg.serial_dependency_secs;
    let longest = tasks.iter().map(|t| t.est_seconds).fold(0.0, f64::max);
    let denom = longest
        + cfg.split_overhead_secs
        + cfg.integrate_overhead_secs
        + cfg.serial_dependency_secs;
    let speedup = if denom > 0.0 { sequential / denom } else { 1.0 };
    if speedup >= cfg.threshold {
        Estimate {
            speedup,
            cost_multiplier: 1.0 + 0.5 * (tasks.len() as f64 - 1.0),
            decision: Decision::Multi,
            reason: format!(
                "{} independent tasks, no write overlap; estimated {:.1}x speedup (>= {:.1}x threshold)",
                tasks.len(),
                speedup,
                cfg.threshold
            ),
        }
    } else {
        Estimate {
            speedup,
            cost_multiplier: 1.0,
            decision: Decision::Single,
            reason: format!(
                "estimated {:.1}x speedup is below the {:.1}x threshold; split/integration overhead dominates",
                speedup, cfg.threshold
            ),
        }
    }
}

/// Extra instruction for the main model. Whether to call `spawn_agent` stays with the model.
pub fn recommendation_text(e: &Estimate) -> String {
    match e.decision {
        Decision::Multi => format!(
            "Team estimate: splitting into multiple agents is recommended ({}; cost ~{:.1}x). Define the shared contract first, give each agent a disjoint set of files to write, then integrate. You decide whether to call spawn_agent.",
            e.reason, e.cost_multiplier
        ),
        Decision::Single => format!(
            "Team estimate: proceed as a single agent ({}). You decide whether to call spawn_agent.",
            e.reason
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: &str, p: &str, s: f64) -> Task {
        Task {
            name: n.into(),
            write_paths: vec![p.into()],
            est_seconds: s,
        }
    }

    #[test]
    fn long_independent_tasks_go_multi() {
        let e = estimate(
            &[t("a", "src/a", 600.0), t("b", "src/b", 600.0)],
            &EstimateConfig::default(),
        );
        assert_eq!(e.decision, Decision::Multi);
        assert!((e.speedup - 1200.0 / 720.0).abs() < 1e-9);
    }

    #[test]
    fn short_tasks_stay_single() {
        let e = estimate(
            &[t("a", "a", 30.0), t("b", "b", 30.0)],
            &EstimateConfig::default(),
        );
        assert_eq!(e.decision, Decision::Single);
    }

    #[test]
    fn overlap_forces_single() {
        let e = estimate(
            &[t("a", "src", 900.0), t("b", "src/x.rs", 900.0)],
            &EstimateConfig::default(),
        );
        assert_eq!(e.decision, Decision::Single);
        assert!(e.reason.contains("overlap"));
    }

    #[test]
    fn one_task_single_and_no_boundary_single() {
        assert_eq!(
            estimate(&[t("a", "a", 900.0)], &EstimateConfig::default()).decision,
            Decision::Single
        );
        let cfg = EstimateConfig {
            boundary_writable: false,
            ..Default::default()
        };
        assert_eq!(
            estimate(&[t("a", "a", 900.0), t("b", "b", 900.0)], &cfg).decision,
            Decision::Single
        );
    }

    #[test]
    fn threshold_is_configurable() {
        let cfg = EstimateConfig {
            threshold: 3.0,
            ..Default::default()
        };
        let e = estimate(&[t("a", "a", 600.0), t("b", "b", 600.0)], &cfg);
        assert_eq!(e.decision, Decision::Single);
    }
}
