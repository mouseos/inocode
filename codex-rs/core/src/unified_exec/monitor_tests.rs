use super::*;
use pretty_assertions::assert_eq;

fn obs(tail: &str, idle_secs: u64) -> Observation {
    Observation {
        exited: false,
        elapsed: Duration::from_secs(idle_secs + 5),
        idle: Duration::from_secs(idle_secs),
        total_bytes: tail.len() as u64,
        tail: tail.to_string(),
        activity: Activity::Unmeasured,
        sample: None,
    }
}

fn with_activity(mut o: Observation, activity: Activity) -> Observation {
    o.activity = activity;
    o
}

#[test]
fn quiet_unknown_command_is_not_an_anomaly_before_threshold() {
    let a = assess(TerminalKind::Unknown, &obs("compiling\n", 300), 1);
    assert_eq!(a.health, Health::Running);
}

#[test]
fn long_silence_of_finite_command_is_stalled() {
    let a = assess(TerminalKind::Unknown, &obs("compiling\n", 601), 1);
    assert_eq!(a.health, Health::Stalled);
    // builds get twice the grace
    assert_eq!(assess(TerminalKind::Build, &obs("x\n", 601), 1).health, Health::Running);
    assert_eq!(assess(TerminalKind::Build, &obs("x\n", 1201), 1).health, Health::Stalled);
}

#[test]
fn servers_and_watchers_may_be_silent_or_repetitive() {
    let repeated = "tick\n".repeat(50);
    for kind in [TerminalKind::Server, TerminalKind::Watch] {
        assert_eq!(assess(kind, &obs("ready\n", 99_999), 1).health, Health::Running);
        assert_eq!(assess(kind, &obs(&repeated, 5), 1).health, Health::Running);
    }
}

#[test]
fn identical_lines_are_a_loop_but_counters_are_progress() {
    let same = "retrying connection\n".repeat(LOOP_MIN_REPEATS);
    assert_eq!(assess(TerminalKind::Unknown, &obs(&same, 1), 1).health, Health::Looping);

    let counting: String = (0..100).map(|i| format!("processed {i} items\n")).collect();
    assert_eq!(assess(TerminalKind::Unknown, &obs(&counting, 1), 1).health, Health::Running);

    let few = "retrying connection\n".repeat(LOOP_MIN_REPEATS - 1);
    assert_eq!(assess(TerminalKind::Unknown, &obs(&few, 1), 1).health, Health::Running);
}

#[test]
fn carriage_return_progress_bars_are_not_repeats() {
    let bar = "10%\r20%\r30%\r40%\r50%";
    assert_eq!(clean_lines(bar), vec!["50%".to_string()]);
}

#[test]
fn prompts_need_idle_time_and_no_trailing_newline() {
    let prompt = "Proceed with install [y/N] ";
    assert_eq!(assess(TerminalKind::Unknown, &obs(prompt, 5), 1).health, Health::Running);
    assert_eq!(
        assess(TerminalKind::Unknown, &obs(prompt, 20), 1).health,
        Health::WaitingForInput
    );
    assert_eq!(
        assess(TerminalKind::Unknown, &obs("Proceed [y/N]\n", 60), 1).health,
        Health::Running
    );
    assert_eq!(
        assess(TerminalKind::Unknown, &obs("Password: ", 20), 1).health,
        Health::WaitingForInput
    );
    assert_eq!(
        assess(TerminalKind::Server, &obs("Enter passphrase: ", 20), 1).health,
        Health::WaitingForInput,
        "a server that asks for a password is still blocked"
    );
}

#[test]
fn exited_process_is_never_an_anomaly() {
    let mut o = obs("Password: ", 999);
    o.exited = true;
    assert_eq!(assess(TerminalKind::Unknown, &o, 1).health, Health::Exited);
}

#[test]
fn kind_is_inferred_from_the_command_line() {
    assert_eq!(TerminalKind::infer("npm run dev"), TerminalKind::Server);
    assert_eq!(TerminalKind::infer("tail -f app.log"), TerminalKind::Watch);
    assert_eq!(TerminalKind::infer("cargo test -p core"), TerminalKind::Test);
    assert_eq!(TerminalKind::infer("cargo build --release"), TerminalKind::Build);
    assert_eq!(TerminalKind::infer("ls -la"), TerminalKind::Unknown);
    assert_eq!(TerminalKind::parse("One-Shot"), Some(TerminalKind::OneShot));
    assert_eq!(TerminalKind::parse("nonsense"), None);
}

#[test]
fn notifications_are_capped_and_deduplicated() {
    let mut state = MonitorState {
        command: "sleep 1000".to_string(),
        ..Default::default()
    };
    let anomaly = |fp: &str| Assessment {
        health: Health::Stalled,
        reason: String::new(),
        fingerprint: fp.to_string(),
    };
    assert!(state.should_notify(&anomaly("a")));
    assert!(!state.should_notify(&anomaly("a")), "same anomaly is not repeated");
    assert!(state.should_notify(&anomaly("b")));
    assert!(state.should_notify(&anomaly("c")));
    assert!(!state.should_notify(&anomaly("d")), "cap of three reached");
    assert_eq!(state.notifications_sent(), MAX_ANOMALY_NOTIFICATIONS);
}

#[test]
fn waiting_silences_the_anomaly_and_doubles_thresholds() {
    let mut state = MonitorState::default();
    let stalled = assess(TerminalKind::Unknown, &obs("x\n", 700), state.scale());
    assert_eq!(stalled.health, Health::Stalled);
    state.acknowledge(&stalled);
    assert!(!state.should_notify(&stalled));
    assert_eq!(state.scale(), 2);
    let later = assess(TerminalKind::Unknown, &obs("x\n", 700), state.scale());
    assert_eq!(later.health, Health::Running, "threshold doubled to 20 minutes");
}

#[test]
fn running_assessment_never_notifies() {
    let mut state = MonitorState::default();
    let running = assess(TerminalKind::Unknown, &obs("hi\n", 1), 1);
    assert!(!state.should_notify(&running));
}

#[test]
fn silent_but_busy_process_is_not_stalled() {
    let busy = with_activity(obs("compiling\n", 1500), Activity::Active);
    assert_eq!(assess(TerminalKind::Unknown, &busy, 1).health, Health::Running);
}

#[test]
fn silent_and_idle_process_is_stalled_only_after_the_confirmation_window() {
    let just_quiet = with_activity(obs("x\n", 700), Activity::Quiet(Duration::from_secs(30)));
    assert_eq!(assess(TerminalKind::Unknown, &just_quiet, 1).health, Health::Running);
    let long_quiet = with_activity(obs("x\n", 700), Activity::Quiet(ACTIVITY_QUIET));
    let verdict = assess(TerminalKind::Unknown, &long_quiet, 1);
    assert_eq!(verdict.health, Health::Stalled);
    assert!(verdict.reason.contains("no CPU or child-process activity"));
}

#[test]
fn repeated_lines_are_a_loop_only_while_the_process_works() {
    let same = "retrying\n".repeat(LOOP_MIN_REPEATS);
    let working = with_activity(obs(&same, 1), Activity::Active);
    assert_eq!(assess(TerminalKind::Unknown, &working, 1).health, Health::Looping);
    let sleeping = with_activity(obs(&same, 1), Activity::Quiet(ACTIVITY_QUIET));
    assert_eq!(assess(TerminalKind::Unknown, &sleeping, 1).health, Health::Running);
}

#[test]
fn huge_output_or_memory_is_runaway_even_for_servers() {
    let mut flood = obs("x\n", 1);
    flood.total_bytes = RUNAWAY_OUTPUT_BYTES;
    assert_eq!(assess(TerminalKind::Server, &flood, 1).health, Health::Runaway);

    let mut fat = obs("x\n", 1);
    fat.sample = Some(ActivitySample {
        cpu_ms: 0,
        processes: 1,
        rss_bytes: RUNAWAY_MEMORY_BYTES,
    });
    assert_eq!(assess(TerminalKind::Watch, &fat, 1).health, Health::Runaway);
}

fn sample(cpu_ms: u64, processes: usize) -> ActivitySample {
    ActivitySample {
        cpu_ms,
        processes,
        rss_bytes: 0,
    }
}

#[tokio::test(start_paused = true)]
async fn cpu_growth_or_new_children_count_as_activity() {
    let mut state = MonitorState::default();
    assert_eq!(state.activity(Instant::now()), Activity::Unmeasured);

    state.record_sample(Instant::now(), sample(100, 1));
    tokio::time::advance(Duration::from_secs(60)).await;
    state.record_sample(Instant::now(), sample(100, 1));
    tokio::time::advance(Duration::from_secs(60)).await;
    state.record_sample(Instant::now(), sample(100, 1));
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(matches!(state.activity(Instant::now()), Activity::Quiet(_)));

    state.record_sample(Instant::now(), sample(500, 1));
    assert_eq!(state.activity(Instant::now()), Activity::Active, "CPU time grew");

    tokio::time::advance(Duration::from_secs(200)).await;
    assert!(matches!(state.activity(Instant::now()), Activity::Quiet(_)));
    state.record_sample(Instant::now(), sample(500, 3));
    assert_eq!(state.activity(Instant::now()), Activity::Active, "children appeared");
}

#[tokio::test(start_paused = true)]
async fn samples_taken_close_together_are_compared_with_the_older_baseline() {
    let mut state = MonitorState::default();
    state.record_sample(Instant::now(), sample(0, 1));
    tokio::time::advance(Duration::from_secs(200)).await;
    state.record_sample(Instant::now(), sample(0, 1));
    tokio::time::advance(Duration::from_secs(1)).await;
    // One second later the CPU counter moved by less than one tick; the baseline is
    // still the sample from 1s ago, so this must not erase the quiet period.
    state.record_sample(Instant::now(), sample(5, 1));
    assert!(matches!(state.activity(Instant::now()), Activity::Quiet(_)));
}
