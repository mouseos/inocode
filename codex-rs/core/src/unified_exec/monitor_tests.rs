use super::*;
use pretty_assertions::assert_eq;

fn obs(tail: &str, idle_secs: u64) -> Observation {
    Observation {
        exited: false,
        elapsed: Duration::from_secs(idle_secs + 5),
        idle: Duration::from_secs(idle_secs),
        total_bytes: tail.len() as u64,
        tail: tail.to_string(),
    }
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
