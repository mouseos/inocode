//! Reads CPU time, memory and process count of a terminal's process tree.

use std::collections::HashSet;

use sysinfo::Pid;
use sysinfo::ProcessRefreshKind;
use sysinfo::ProcessesToUpdate;
use sysinfo::System;

use super::monitor::ActivitySample;

/// Sum CPU time and memory over `pid` and all of its descendants.
///
/// Blocking; call from `spawn_blocking`. Returns `None` once the root process is gone.
pub(crate) fn sample_process_tree(pid: u32) -> Option<ActivitySample> {
    let Ok(mut system) = System::new() else {
        return None;
    };
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        /*remove_dead_processes*/ true,
        ProcessRefreshKind::nothing().with_cpu().with_memory(),
    );
    let root = Pid::from_u32(pid);
    system.process(root)?;

    let mut members: HashSet<Pid> = HashSet::from([root]);
    loop {
        let before = members.len();
        for (candidate, process) in system.processes() {
            if process
                .parent()
                .is_some_and(|parent| members.contains(&parent))
            {
                members.insert(*candidate);
            }
        }
        if members.len() == before {
            break;
        }
    }

    let mut sample = ActivitySample::default();
    for member in &members {
        if let Some(process) = system.process(*member) {
            sample.cpu_ms = sample.cpu_ms.saturating_add(process.accumulated_cpu_time());
            sample.rss_bytes = sample.rss_bytes.saturating_add(process.memory());
            sample.processes += 1;
        }
    }
    Some(sample)
}
