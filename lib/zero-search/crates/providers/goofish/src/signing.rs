//! Signer provider abstraction — port of `goofish_client/signing.py`
//! and `goofish_client/serve_signer.py`. The SecurityGuard final
//! computation is native and is NOT reimplemented here; this module
//! assembles the INPUT string exactly and bridges to caller-owned
//! external signing processes. No fake signature is ever produced.

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::protocol::PUBLIC_APP_KEY;

// ---------------------------------------------------------------------------
// SignRequest / SignResult
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SignRequest {
    pub api: String,
    pub version: String,
    /// The request JSON string (the plan's `data_json()`).
    pub data: String,
    pub timestamp: String,
    pub utdid: Option<String>,
    pub device_id: Option<String>,
    pub ttid: Option<String>,
    pub uid: Option<String>,
    pub sid: Option<String>,
    pub app_key: String,
    pub reqbiz_ext: Option<String>,
    pub lat: Option<String>,
    pub lng: Option<String>,
    pub extdata: Option<String>,
    pub x_features: Option<String>,
    pub router_id: Option<String>,
    pub place_id: Option<String>,
    pub open_biz: Option<String>,
    pub mini_app_key: Option<String>,
    pub req_app_key: Option<String>,
    pub access_token: Option<String>,
    pub open_biz_data: Option<String>,
    /// Unified signing (pv 6.3) always includes the extdata slot;
    /// legacy signing (pv 6.2) includes it only when extdata non-empty.
    pub include_ext_data: bool,
    pub use_wua: bool,
    pub env: i64,
    pub request_id: Option<String>,
}

impl SignRequest {
    pub fn new(api: &str, version: &str, data: String, timestamp: String) -> Self {
        Self {
            api: api.to_string(),
            version: version.to_string(),
            data,
            timestamp,
            utdid: None,
            device_id: None,
            ttid: None,
            uid: None,
            sid: None,
            app_key: PUBLIC_APP_KEY.to_string(),
            reqbiz_ext: None,
            lat: None,
            lng: None,
            extdata: None,
            x_features: None,
            router_id: None,
            place_id: None,
            open_biz: None,
            mini_app_key: None,
            req_app_key: None,
            access_token: None,
            open_biz_data: None,
            include_ext_data: true,
            use_wua: false,
            env: 0,
            request_id: None,
        }
    }

    pub fn data_json(&self) -> String {
        self.data.clone()
    }
}

#[derive(Debug, Clone, Default)]
pub struct SignResult {
    pub sign: String,
    pub x_umt: Option<String>,
    pub x_mini_wua: Option<String>,
    pub x_sgext: Option<String>,
    pub wua: Option<String>,
    pub extras: Map<String, Value>,
}

pub trait Signer: Send + Sync {
    fn sign(&self, request: &SignRequest) -> Result<SignResult>;
}

fn s(value: &Option<String>) -> String {
    value.clone().unwrap_or_default()
}

/// Assemble the SecurityGuard INPUT string (mtop-signing.md §2):
/// `&`-joined, null -> "", MD5 hex of the data JSON in place of data,
/// api/v lower-cased. Slot order:
/// utdid & uid & reqbiz-ext & appKey & MD5(data) & t & api & v & sid
/// & ttid & deviceId & lat & lng & [extdata &] & x-features & routerId
/// & placeId & openBiz & miniAppKey & reqAppKey & accessToken & openBizData
pub fn build_sign_input(request: &SignRequest, include_ext_data: Option<bool>) -> String {
    let include_ext_data = include_ext_data.unwrap_or(request.include_ext_data);
    let data_md5 = format!("{:x}", md5::compute(request.data_json().as_bytes()));
    let mut parts: Vec<String> = vec![
        s(&request.utdid),
        s(&request.uid),
        s(&request.reqbiz_ext),
        request.app_key.clone(),
        data_md5,
        request.timestamp.clone(),
        request.api.to_lowercase(),
        request.version.to_lowercase(),
        s(&request.sid),
        s(&request.ttid),
        s(&request.device_id),
        s(&request.lat),
        s(&request.lng),
    ];
    if include_ext_data
        || request
            .extdata
            .as_deref()
            .map(|e| !e.is_empty())
            .unwrap_or(false)
    {
        parts.push(s(&request.extdata));
    }
    parts.extend([
        s(&request.x_features),
        s(&request.router_id),
        s(&request.place_id),
        s(&request.open_biz),
        s(&request.mini_app_key),
        s(&request.req_app_key),
        s(&request.access_token),
        s(&request.open_biz_data),
    ]);
    parts.join("&")
}

/// The JSON object a signing bridge sends to an external signer.
pub fn sign_request_payload(request: &SignRequest) -> Value {
    let mut payload = Map::new();
    payload.insert("api".into(), json!(request.api));
    payload.insert("version".into(), json!(request.version));
    payload.insert("data".into(), json!(request.data_json()));
    payload.insert("t".into(), json!(request.timestamp));
    payload.insert("timestamp".into(), json!(request.timestamp));
    payload.insert("utdid".into(), opt_or_null(&request.utdid));
    payload.insert("device_id".into(), opt_or_null(&request.device_id));
    payload.insert("ttid".into(), opt_or_null(&request.ttid));
    payload.insert("uid".into(), opt_or_null(&request.uid));
    payload.insert("sid".into(), opt_or_null(&request.sid));
    payload.insert("app_key".into(), json!(request.app_key));
    payload.insert("input".into(), json!(build_sign_input(request, None)));
    payload.insert("include_ext_data".into(), json!(request.include_ext_data));
    payload.insert("use_wua".into(), json!(request.use_wua));
    payload.insert("env".into(), json!(request.env));
    let optional: Vec<(&str, &Option<String>)> = vec![
        ("reqbiz_ext", &request.reqbiz_ext),
        ("lat", &request.lat),
        ("lng", &request.lng),
        ("extdata", &request.extdata),
        ("x_features", &request.x_features),
        ("router_id", &request.router_id),
        ("place_id", &request.place_id),
        ("open_biz", &request.open_biz),
        ("mini_app_key", &request.mini_app_key),
        ("req_app_key", &request.req_app_key),
        ("access_token", &request.access_token),
        ("open_biz_data", &request.open_biz_data),
        ("request_id", &request.request_id),
    ];
    for (name, value) in optional {
        if let Some(v) = value {
            payload.insert(name.into(), json!(v));
        }
    }
    Value::Object(payload)
}

fn opt_or_null(value: &Option<String>) -> Value {
    match value {
        Some(v) => json!(v),
        None => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// UnavailableSigner
// ---------------------------------------------------------------------------

pub struct UnavailableSigner {
    pub reason: String,
}

impl Default for UnavailableSigner {
    fn default() -> Self {
        Self {
            reason: "no signing provider is configured: the SecurityGuard final computation is native and is not reproduced by this package".to_string(),
        }
    }
}

impl Signer for UnavailableSigner {
    fn sign(&self, request: &SignRequest) -> Result<SignResult> {
        Err(anyhow!(
            "refusing to sign {}/{}: {}",
            request.api,
            request.version,
            self.reason
        ))
    }
}

// ---------------------------------------------------------------------------
// SubprocessSigner (fresh process per call, JSON-lines)
// ---------------------------------------------------------------------------

pub struct SubprocessSigner {
    pub command: Vec<String>,
    pub timeout: Duration,
}

impl SubprocessSigner {
    pub fn new(command: Vec<String>) -> Result<Self> {
        Self::with_timeout(command, Duration::from_secs(10))
    }

    pub fn with_timeout(command: Vec<String>, timeout: Duration) -> Result<Self> {
        if command.is_empty() || command.iter().any(|c| c.is_empty()) {
            return Err(anyhow!(
                "SubprocessSigner command must be a non-empty sequence of strings"
            ));
        }
        if timeout.is_zero() {
            return Err(anyhow!("SubprocessSigner timeout must be positive"));
        }
        Ok(Self { command, timeout })
    }
}

impl Signer for SubprocessSigner {
    fn sign(&self, request: &SignRequest) -> Result<SignResult> {
        let payload_line = format!("{}\n", sign_request_payload(request));
        let mut child = Command::new(&self.command[0])
            .args(&self.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                anyhow!(
                    "signing process could not be started ({}): startup",
                    class_name(&e)
                )
            })?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload_line.as_bytes());
            // stdin dropped here -> closed, as the protocol requires.
        }
        let stdout = child.stdout.take();
        let (tx, rx) = channel::<String>();
        let reader = std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(out) = stdout {
                let mut reader = BufReader::new(out);
                let _ = reader.read_line(&mut buf);
            }
            let _ = tx.send(buf);
        });
        let line = match rx.recv_timeout(self.timeout) {
            Ok(l) => l,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!(
                    "signing process timed out after {}s: timeout",
                    self.timeout.as_secs_f64()
                ));
            }
        };
        let _ = reader.join();
        let status = child
            .wait()
            .map_err(|e| anyhow!("signing process wait failed: {e}"))?;
        if !status.success() {
            return Err(anyhow!(
                "signing process exited abnormally (returncode {}): exit",
                status.code().unwrap_or(-1)
            ));
        }
        let line = line.trim();
        if line.is_empty() {
            return Err(anyhow!("signing process produced no output: malformed"));
        }
        let doc: Value = serde_json::from_str(line)
            .map_err(|_| anyhow!("signing process returned malformed JSON: malformed"))?;
        let doc = doc.as_object().ok_or_else(|| {
            anyhow!("signing process returned a non-object JSON value: malformed")
        })?;
        let pick = |names: &[&str]| -> Option<String> {
            for name in names {
                if let Some(Value::String(v)) = doc.get(*name) {
                    if !v.is_empty() {
                        return Some(v.clone());
                    }
                }
            }
            None
        };
        let sign_value = pick(&["sign", "x-sign", "x_sign"])
            .ok_or_else(|| anyhow!("signing process returned no sign value: no_sign"))?;
        let known = [
            "sign",
            "x-sign",
            "x_sign",
            "x_umt",
            "x-umt",
            "x_mini_wua",
            "x-mini-wua",
            "x_sgext",
            "x-sgext",
            "wua",
        ];
        let mut extras = Map::new();
        for (k, v) in doc {
            if !known.contains(&k.as_str()) {
                extras.insert(k.clone(), v.clone());
            }
        }
        Ok(SignResult {
            sign: sign_value,
            x_umt: pick(&["x_umt", "x-umt"]),
            x_mini_wua: pick(&["x_mini_wua", "x-mini-wua"]),
            x_sgext: pick(&["x_sgext", "x-sgext"]),
            wua: pick(&["wua"]),
            extras,
        })
    }
}

fn class_name(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::NotFound => "FileNotFoundError",
        std::io::ErrorKind::PermissionDenied => "PermissionError",
        _ => "OSError",
    }
}

// ---------------------------------------------------------------------------
// ServeSigner (persistent serve process)
// ---------------------------------------------------------------------------

enum LineEvent {
    Line(String),
    Eof,
}

/// Marker error for "the harness process died" (its output closed
/// underneath us, or it was already gone when we looked). Kept
/// distinct from every deterministic failure — a malformed ready
/// line, an SG init rejection, a protocol error reply — because
/// death is transient in a way those are not: the process may
/// have been OOM-killed under host memory pressure (the harness
/// tree was SIGKILLed exactly that way in round 19) or killed
/// with its process group, and a fresh instance serves normally.
/// `ServeSigner::sign` restarts a dead harness a bounded number
/// of times; deterministic failures latch exactly as before. This
/// is about the local subprocess only — it never retries an MTOP
/// send, so it cannot lift a server-side send halt.
#[derive(Debug)]
struct HarnessDied(String);

impl std::fmt::Display for HarnessDied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HarnessDied {}

struct UnidbgInner {
    child: Option<Child>,
    rx: Option<Receiver<LineEvent>>,
    broken: Option<String>,
    closed: bool,
    /// How many times a dead harness has been restarted in this
    /// signer (bounded by `MAX_HARNESS_RESTARTS`).
    restarts_used: u32,
    /// Wall time the most recent harness start took (ready line
    /// received), for the phase breakdown in issue #2.
    startup_duration: Option<Duration>,
    /// Release latch for the harness's creator thread (see
    /// `spawn_harness`). Dropped in `shutdown()` only, after the
    /// child has been killed: while the latch is held, the creator
    /// thread stays alive and PR_SET_PDEATHSIG cannot fire.
    spawner_release: Option<Sender<()>>,
}

/// Spawn the harness command on a dedicated creator thread and
/// return the child plus the latch that keeps that thread alive.
///
/// Why the indirection (K1-4): on Linux the harness is spawned
/// with PR_SET_PDEATHSIG so the kernel kills it when its parent
/// dies without cleanup — but for PDEATHSIG the "parent" is the
/// *thread that created the process*, not the process as a whole.
/// Callers may trigger the start from a short-lived thread (the
/// server's pre-warm thread returns as soon as the start
/// completes), and the kernel then SIGKILLed the harness the
/// moment that thread exited — after ready, before the first
/// sign, with stderr discarded so the death looked silent. The
/// creator thread therefore lives until `shutdown()` drops the
/// latch (after the child is dead), so PDEATHSIG only ever fires
/// when the whole signer is gone, which is the case it exists
/// for (server killed abruptly, client vanished).
#[cfg(unix)]
fn spawn_harness(mut command: Command) -> std::io::Result<(Child, Sender<()>)> {
    let (child_tx, child_rx) = channel::<std::io::Result<Child>>();
    let (release_tx, release_rx) = channel::<()>();
    std::thread::spawn(move || {
        let result = command.spawn();
        let spawned = result.is_ok();
        let _ = child_tx.send(result);
        if spawned {
            // Park until shutdown() drops the latch (or every
            // sender is gone): exiting earlier would deliver the
            // PDEATHSIG to the harness.
            let _ = release_rx.recv();
        }
    });
    match child_rx.recv() {
        Ok(Ok(child)) => Ok((child, release_tx)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(std::io::Error::other(
            "harness creator thread died before spawning",
        )),
    }
}

#[cfg(not(unix))]
fn spawn_harness(mut command: Command) -> std::io::Result<(Child, Sender<()>)> {
    // No PDEATHSIG off Unix: spawn directly. The latch is inert.
    let (release_tx, _release_rx) = channel::<()>();
    let child = command.spawn()?;
    Ok((child, release_tx))
}

/// Short human-readable form of a reaped child's exit status, for
/// error messages: metadata only (code / signal), never output.
fn describe_exit(status: &std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("killed by signal {sig}");
        }
    }
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => status.to_string(),
    }
}

/// How many times one signer restarts a harness that *died*
/// (process exit) before giving up and latching the death as its
/// error. Deterministic failures never consume restarts.
const MAX_HARNESS_RESTARTS: u32 = 3;

pub struct ServeSigner {
    pub command: Vec<String>,
    pub timeout: Duration,
    /// Budget for the harness *startup* (spawn -> ready line). Kept
    /// separate from `timeout` (the per-sign-request budget): startup
    /// runs the whole SecurityGuard init under emulation and is far
    /// slower and far more variable than a warm sign — measured
    /// ~150-180s on an idle VM (round 19) but ~420-460s under load
    /// (2026-11-07), which a single shared 300s budget did not cover.
    pub startup_timeout: Duration,
    pub request_type: i64,
    pub auth_code: String,
    /// INPUT layout selector (default false = round-19 legacy path B, pv 6.2).
    pub include_ext_data: bool,
    inner: Mutex<UnidbgInner>,
}

impl ServeSigner {
    pub fn new(command: Vec<String>) -> Result<Self> {
        Self::with_options(command, Duration::from_secs(120), false)
    }

    pub fn with_options(
        command: Vec<String>,
        timeout: Duration,
        include_ext_data: bool,
    ) -> Result<Self> {
        Self::with_timeouts(command, timeout, timeout, include_ext_data)
    }

    pub fn with_timeouts(
        command: Vec<String>,
        timeout: Duration,
        startup_timeout: Duration,
        include_ext_data: bool,
    ) -> Result<Self> {
        if command.is_empty() || command.iter().any(|c| c.is_empty()) {
            return Err(anyhow!(
                "ServeSigner command must be a non-empty sequence of strings"
            ));
        }
        if timeout.is_zero() || startup_timeout.is_zero() {
            return Err(anyhow!("ServeSigner timeout must be positive"));
        }
        Ok(Self {
            command,
            timeout,
            startup_timeout,
            request_type: 7,
            auth_code: String::new(),
            include_ext_data,
            inner: Mutex::new(UnidbgInner {
                child: None,
                rx: None,
                broken: None,
                closed: false,
                restarts_used: 0,
                startup_duration: None,
                spawner_release: None,
            }),
        })
    }

    /// Reap the harness child if it has exited and describe how
    /// (exit code / signal). EOF on the output pipe means every
    /// writer is gone, but the process may take a moment to become
    /// reapable, so poll briefly. Returns None when the child is
    /// somehow still running (it closed its output but lives) —
    /// the error then stays in its historic form. This turns the
    /// formerly silent death ("closed its output", stderr nulled)
    /// into a diagnosed one: signal 9 reads very differently from
    /// exit code 1.
    fn reap_status(inner: &mut UnidbgInner) -> Option<String> {
        let child = inner.child.as_mut()?;
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(describe_exit(&status)),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return None,
            }
        }
    }

    fn read_line(inner: &mut UnidbgInner, timeout: Duration, phase: &str) -> Result<String> {
        // Take the receiver out so the Eof path can mutably reap
        // the child for its exit status; put it back before
        // returning (a failed read latches the signer broken and
        // shutdown() drops it anyway).
        let rx = inner.rx.take().expect("line channel exists after start");
        let received = rx.recv_timeout(timeout);
        inner.rx = Some(rx);
        match received {
            Ok(LineEvent::Line(l)) => Ok(l),
            Ok(LineEvent::Eof) => {
                let detail = Self::reap_status(inner)
                    .map(|s| format!(" ({s})"))
                    .unwrap_or_default();
                // The process is gone: a death, not a verdict.
                // Typed so `sign` can restart the harness instead
                // of latching (see HarnessDied).
                Err(HarnessDied(format!(
                    "signing harness closed its output during {phase}{detail}: exit"
                ))
                .into())
            }
            Err(_) => Err(anyhow!(
                "signing harness timed out during {phase} after {}s: timeout",
                timeout.as_secs_f64()
            )),
        }
    }

    fn start(&self, inner: &mut UnidbgInner) -> Result<()> {
        let started_at = Instant::now();
        let mut command = Command::new(&self.command[0]);
        command
            .args(&self.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Orphan prevention (issue #2): the harness runs in its own
        // process group so shutdown can kill the whole tree, and on
        // Linux it gets PR_SET_PDEATHSIG so the kernel kills it even
        // when this process dies without running any cleanup (e.g.
        // SIGKILL after the MCP client vanished).
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            unsafe {
                command.pre_exec(|| {
                    // PR_SET_PDEATHSIG is Linux-only; on other Unix
                    // platforms the process-group kill above is the
                    // orphan-prevention mechanism.
                    #[cfg(target_os = "linux")]
                    {
                        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        // Race: the parent may already be gone.
                        if libc::getppid() == 1 {
                            libc::_exit(1);
                        }
                    }
                    Ok(())
                });
            }
        }
        let (mut child, release) = spawn_harness(command).map_err(|e| {
            anyhow!(
                "signing harness could not be started ({}): startup",
                class_name(&e)
            )
        })?;
        let stdout = child.stdout.take().expect("stdout piped");
        let (tx, rx) = channel::<LineEvent>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(LineEvent::Eof);
                        break;
                    }
                    Ok(_) => {
                        if tx.send(LineEvent::Line(line)).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        inner.child = Some(child);
        inner.spawner_release = Some(release);
        inner.rx = Some(rx);
        let line = Self::read_line(inner, self.startup_timeout, "startup")?;
        let ready: Value = serde_json::from_str(line.trim())
            .map_err(|_| anyhow!("signing harness sent a malformed ready line: startup"))?;
        let ready_obj = ready
            .as_object()
            .ok_or_else(|| anyhow!("signing harness did not report ready: startup"))?;
        if ready_obj.get("ready").and_then(|v| v.as_bool()) != Some(true) {
            return Err(anyhow!("signing harness did not report ready: startup"));
        }
        // Round 28: ready=true alone does not mean usable — validate
        // initRc / appKey when present (absent keys are accepted).
        if let Some(init_rc) = ready_obj.get("initRc") {
            if init_rc.as_i64() != Some(0) {
                let detail = init_rc
                    .as_i64()
                    .map(|n| format!(" (initRc={n})"))
                    .unwrap_or_default();
                return Err(anyhow!(
                    "signing harness reported SG init failure{detail}: startup"
                ));
            }
        }
        if let Some(app_key) = ready_obj.get("appKey") {
            let ok = matches!(app_key, Value::String(s) if !s.is_empty());
            if !ok {
                return Err(anyhow!(
                    "signing harness reported no appKey at startup: startup"
                ));
            }
        }
        inner.startup_duration = Some(started_at.elapsed());
        Ok(())
    }

    /// Start the harness now, without signing anything (pre-warm).
    /// A no-op when already started. A deterministic failure (bad
    /// ready line, SG init rejection, startup timeout) is latched
    /// exactly like a failed first sign: the signer is marked
    /// broken and the harness is shut down, so a later sign
    /// surfaces the same error instead of silently retrying the
    /// start. A start that ended in the harness's *death* is not
    /// latched: the error is returned, and a later sign restarts
    /// the harness (bounded by MAX_HARNESS_RESTARTS across the
    /// signer's lifetime).
    pub fn ensure_started(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(anyhow!("serve signer is closed"));
        }
        if let Some(broken) = &inner.broken {
            return Err(anyhow!("{broken}"));
        }
        if inner.child.is_none() {
            if let Err(e) = self.start(&mut inner) {
                let died = e.downcast_ref::<HarnessDied>().is_some();
                let msg = e.to_string();
                Self::shutdown(&mut inner);
                if died && inner.restarts_used < MAX_HARNESS_RESTARTS {
                    inner.restarts_used += 1;
                    return Err(anyhow!("{msg}"));
                }
                inner.broken = Some(msg.clone());
                return Err(anyhow!("{msg}"));
            }
        }
        Ok(())
    }

    /// Whether the harness process has been started (and not shut
    /// down since).
    pub fn is_started(&self) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.child.is_some())
            .unwrap_or(false)
    }

    /// How long the most recent harness start took, if it has
    /// started at least once in this signer.
    pub fn last_startup_duration(&self) -> Option<Duration> {
        self.inner.lock().ok()?.startup_duration
    }

    fn shutdown(inner: &mut UnidbgInner) {
        inner.rx = None;
        if let Some(mut child) = inner.child.take() {
            let pid = child.id();
            if let Some(stdin) = child.stdin.take() {
                drop(stdin);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if Instant::now() > deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
            // The harness leads its own process group (see start()):
            // kill the group as well, so grandchildren (a Java
            // harness spawned under a wrapper shell that did not
            // exec, helper processes, ...) cannot outlive the signer
            // even when the direct child already exited cleanly.
            #[cfg(unix)]
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
            let _ = pid;
        }
        // Only now that the child is dead may its creator thread
        // exit: dropping the latch releases that thread, and its
        // exit can no longer deliver PR_SET_PDEATHSIG to a live
        // harness.
        inner.spawner_release = None;
    }

    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        Self::shutdown(&mut inner);
    }

    fn sign_locked(&self, inner: &mut UnidbgInner, request: &SignRequest) -> Result<SignResult> {
        if inner.child.is_none() {
            self.start(inner)?;
        }
        {
            let child = inner.child.as_mut().expect("child after start");
            if let Ok(Some(status)) = child.try_wait() {
                let detail = describe_exit(&status);
                return Err(HarnessDied(format!(
                    "signing harness is no longer running ({detail}): exit"
                ))
                .into());
            }
        }
        let input_str = build_sign_input(request, Some(self.include_ext_data));
        let payload = json!({
            "input": input_str,
            "requestType": self.request_type,
            "authCode": self.auth_code,
        });
        {
            let child = inner.child.as_mut().expect("child after start");
            let stdin = child.stdin.as_mut().expect("stdin piped");
            stdin
                .write_all(format!("{payload}\n").as_bytes())
                .and_then(|_| stdin.flush())
                .map_err(|e| {
                    // A broken pipe here means the harness is gone.
                    HarnessDied(format!(
                        "could not write to the signing harness ({}): exit",
                        class_name(&e)
                    ))
                })?;
        }
        let line = Self::read_line(inner, self.timeout, "sign")?;
        let doc: Value = serde_json::from_str(line.trim())
            .map_err(|_| anyhow!("signing harness returned malformed JSON: malformed"))?;
        let doc = doc.as_object().ok_or_else(|| {
            anyhow!("signing harness returned a non-object JSON value: malformed")
        })?;
        if let Some(err) = doc.get("error") {
            let truthy = match err {
                Value::String(s) => !s.is_empty(),
                Value::Null => false,
                Value::Bool(b) => *b,
                _ => true,
            };
            if truthy {
                // The harness's error text is deliberately not propagated.
                return Err(anyhow!(
                    "signing harness reported an error for the sign request: protocol"
                ));
            }
        }
        let sign_value = match doc.get("sign") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => return Err(anyhow!("signing harness returned no sign value: no_sign")),
        };
        Ok(SignResult {
            sign: sign_value,
            ..Default::default()
        })
    }
}

impl Signer for ServeSigner {
    fn sign(&self, request: &SignRequest) -> Result<SignResult> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(anyhow!("serve signer is closed"));
        }
        if let Some(broken) = &inner.broken {
            return Err(anyhow!("{broken}"));
        }
        loop {
            match self.sign_locked(&mut inner, request) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    // A harness that *died* (process exit — see
                    // HarnessDied) is restarted in place, bounded
                    // by MAX_HARNESS_RESTARTS: a fresh instance is
                    // a new local subprocess, not a new network
                    // identity, and sign() performs no MTOP send,
                    // so this cannot lift a server-side send halt.
                    // Every other failure is deterministic — the
                    // same start or request would fail the same
                    // way — and latches exactly as before.
                    let died = e.downcast_ref::<HarnessDied>().is_some();
                    let msg = e.to_string();
                    Self::shutdown(&mut inner);
                    if died && inner.restarts_used < MAX_HARNESS_RESTARTS {
                        inner.restarts_used += 1;
                        continue;
                    }
                    inner.broken = Some(msg.clone());
                    return Err(anyhow!("{msg}"));
                }
            }
        }
    }
}

impl Drop for ServeSigner {
    /// Dropping the signer (client dropped, server shutting down)
    /// must not leave the harness tree running (issue #2). The
    /// PDEATHSIG set at spawn covers abrupt process death; this
    /// covers every orderly path.
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.closed = true;
            Self::shutdown(&mut inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn req() -> SignRequest {
        let mut r = SignRequest::new(
            "mtop.taobao.idlemtopsearch.search.suggest",
            "4.0",
            "{\"inputWords\":\"x\"}".to_string(),
            "1700000000".to_string(),
        );
        r.utdid = Some("UTDID-1".into());
        r.device_id = Some("DEV-1".into());
        r.ttid = Some("TTID-1".into());
        r
    }

    // Ported from goofish-client/tests/test_round19_signing.py
    #[test]
    fn input_string_full_layout() {
        let md5 = format!("{:x}", md5::compute(b"{\"inputWords\":\"x\"}"));
        let mut expected_parts = vec![
            "UTDID-1",
            "",
            "",
            "21407387",
            md5.as_str(),
            "1700000000",
            "mtop.taobao.idlemtopsearch.search.suggest",
            "4.0",
            "",
            "TTID-1",
            "DEV-1",
            "",
            "",
            "", // lat, lng, extdata (unified: always present)
        ];
        expected_parts.extend(["", "", "", "", "", "", "", ""]); // x-features .. openBizData
        let expected = expected_parts.join("&");
        assert_eq!(expected.split('&').count(), 22);
        assert_eq!(build_sign_input(&req(), None), expected);
    }

    #[test]
    fn input_string_uses_md5_of_data() {
        let result = build_sign_input(&req(), None);
        assert!(result.contains(&format!("{:x}", md5::compute(b"{\"inputWords\":\"x\"}"))));
        assert!(!result.contains("inputWords"));
    }

    #[test]
    fn input_string_nulls_and_lowercase() {
        let mut r = req();
        r.utdid = None;
        r.device_id = None;
        r.ttid = None;
        let input = build_sign_input(&r, None);
        let parts: Vec<&str> = input.split('&').collect();
        assert_eq!(parts[0], "");
        assert_eq!(parts[9], "");
        assert_eq!(parts[10], "");
        assert_eq!(parts[3], "21407387");
        let mut r2 = req();
        r2.api = "MTOP.TAOBAO.UPPER".into();
        r2.version = "V2".into();
        let input2 = build_sign_input(&r2, None);
        let parts: Vec<&str> = input2.split('&').collect();
        assert_eq!(parts[6], "mtop.taobao.upper");
        assert_eq!(parts[7], "v2");
    }

    #[test]
    fn input_string_extdata_rules() {
        let with_slot = build_sign_input(&req(), Some(true));
        assert_eq!(with_slot.split('&').count(), 22);
        let mut r = req();
        r.extdata = Some("EXT".into());
        assert_eq!(
            build_sign_input(&r, Some(true))
                .split('&')
                .collect::<Vec<_>>()[13],
            "EXT"
        );
        let without = build_sign_input(&req(), Some(false));
        assert_eq!(without.split('&').count(), 21);
        assert_eq!(build_sign_input(&r, Some(false)).split('&').count(), 22);
    }

    #[test]
    fn input_string_all_optional_fields_in_order() {
        let mut r = req();
        r.uid = Some("UID".into());
        r.sid = Some("SID".into());
        r.reqbiz_ext = Some("RB".into());
        r.lat = Some("1.5".into());
        r.lng = Some("2.5".into());
        r.x_features = Some("123".into());
        r.router_id = Some("R".into());
        r.place_id = Some("P".into());
        r.open_biz = Some("OB".into());
        r.mini_app_key = Some("MAK".into());
        r.req_app_key = Some("RAK".into());
        r.access_token = Some("AT".into());
        r.open_biz_data = Some("OBD".into());
        r.extdata = Some("EX".into());
        let parts: Vec<String> = build_sign_input(&r, None)
            .split('&')
            .map(|s| s.to_string())
            .collect();
        assert_eq!(parts[1], "UID");
        assert_eq!(parts[8], "SID");
        assert_eq!(parts[2], "RB");
        assert_eq!(parts[11], "1.5");
        assert_eq!(parts[12], "2.5");
        assert_eq!(parts[13], "EX");
        assert_eq!(
            parts[14..],
            ["123", "R", "P", "OB", "MAK", "RAK", "AT", "OBD"]
        );
    }

    #[test]
    fn payload_carries_input_and_components() {
        let payload = sign_request_payload(&req());
        assert_eq!(payload["input"], json!(build_sign_input(&req(), None)));
        assert_eq!(
            payload["api"],
            json!("mtop.taobao.idlemtopsearch.search.suggest")
        );
        assert_eq!(payload["t"], json!("1700000000"));
        assert_eq!(payload["app_key"], json!("21407387"));
    }

    #[test]
    fn unavailable_signer_always_stops() {
        assert!(UnavailableSigner::default().sign(&req()).is_err());
    }

    fn write_script(body: &str, name: &str) -> Vec<String> {
        let dir =
            std::env::temp_dir().join(format!("gf_sign_test_{}_{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("script.py");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        vec!["python3".to_string(), path.to_string_lossy().to_string()]
    }

    const ECHO_SIGNER: &str = r#"
import json, sys
line = sys.stdin.readline()
req = json.loads(line)
assert req["input"], "payload must carry the INPUT string"
json.dump({"sign": "TESTSIGN-" + req["api"], "x-umt": "UMT-TEST", "extra_field": 7}, sys.stdout)
sys.stdout.write("\n")
"#;

    #[test]
    fn subprocess_signer_round_trip() {
        let signer = SubprocessSigner::new(write_script(ECHO_SIGNER, "echo")).unwrap();
        let result = signer.sign(&req()).unwrap();
        assert_eq!(
            result.sign,
            "TESTSIGN-mtop.taobao.idlemtopsearch.search.suggest"
        );
        assert_eq!(result.x_umt.as_deref(), Some("UMT-TEST"));
        assert_eq!(result.extras.get("extra_field"), Some(&json!(7)));
    }

    #[test]
    fn subprocess_signer_hyphenated_keys() {
        let body = "import json,sys; sys.stdin.readline(); print(json.dumps({\"x-sign\":\"HS\",\"x-mini-wua\":\"MW\"}))";
        let signer = SubprocessSigner::new(write_script(body, "hyph")).unwrap();
        let result = signer.sign(&req()).unwrap();
        assert_eq!(result.sign, "HS");
        assert_eq!(result.x_mini_wua.as_deref(), Some("MW"));
    }

    #[test]
    fn subprocess_signer_timeout() {
        let signer = SubprocessSigner::with_timeout(
            write_script("import time; time.sleep(30)", "slow"),
            Duration::from_millis(500),
        )
        .unwrap();
        let err = signer.sign(&req()).unwrap_err().to_string();
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn subprocess_signer_abnormal_exit_leaks_no_secrets() {
        let signer =
            SubprocessSigner::new(write_script("import sys; sys.exit(3)", "exit3")).unwrap();
        let mut r = req();
        r.utdid = Some("UTDID-SECRET-VALUE".into());
        let err = signer.sign(&r).unwrap_err().to_string();
        assert!(err.contains("returncode 3"), "{err}");
        assert!(!err.contains("UTDID-SECRET-VALUE"));
    }

    #[test]
    fn subprocess_signer_missing_sign_and_malformed() {
        let signer = SubprocessSigner::new(write_script(
            "import sys; sys.stdin.readline(); print(\"{}\")",
            "nosign",
        ))
        .unwrap();
        assert!(signer.sign(&req()).is_err());
        let signer = SubprocessSigner::new(write_script(
            "import sys; sys.stdin.readline(); print(\"not json\")",
            "badjson",
        ))
        .unwrap();
        assert!(signer.sign(&req()).is_err());
        assert!(SubprocessSigner::new(vec![]).is_err());
    }

    #[cfg(unix)]
    fn write_shell_script(body: &str, name: &str) -> Vec<String> {
        let dir =
            std::env::temp_dir().join(format!("gf_serve_test_{}_{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("serve.sh");
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        vec![path.to_string_lossy().to_string()]
    }

    #[cfg(unix)]
    #[test]
    fn serve_signer_fake_harness_round_trip() {
        // Fake serve harness: ready line, then one {"sign": ...} per request line.
        let script = "#!/bin/sh\necho '{\"ready\": true, \"initRc\": 0, \"appKey\": \"21407387\"}'\nwhile IFS= read -r line; do echo '{\"sign\": \"FAKESIGN\"}'; done\n";
        let signer = ServeSigner::with_options(
            write_shell_script(script, "ok"),
            Duration::from_secs(10),
            false,
        )
        .unwrap();
        let result = signer.sign(&req()).unwrap();
        assert_eq!(result.sign, "FAKESIGN");
        // persistent process: second sign reuses it
        let result2 = signer.sign(&req()).unwrap();
        assert_eq!(result2.sign, "FAKESIGN");
        signer.close();
    }

    #[cfg(unix)]
    #[test]
    fn serve_signer_survives_spawner_thread_exit() {
        // Regression test (K1-4): the server pre-warms the harness
        // from a short-lived background thread that exits as soon as
        // the start completes. PR_SET_PDEATHSIG is delivered when
        // the thread that *created* the process exits — not when
        // the process exits — so a harness spawned by that thread
        // was SIGKILLed the moment pre-warm finished, and the first
        // real sign then failed with "closed its output during
        // sign". The harness must outlive the thread that happened
        // to trigger its start.
        use std::sync::Arc;
        let script = "#!/bin/sh\necho '{\"ready\": true, \"initRc\": 0, \"appKey\": \"21407387\"}'\nwhile IFS= read -r line; do echo '{\"sign\": \"FAKESIGN\"}'; done\n";
        let signer = Arc::new(
            ServeSigner::with_options(
                write_shell_script(script, "threadexit"),
                Duration::from_secs(10),
                false,
            )
            .unwrap(),
        );
        let warm = signer.clone();
        let handle = std::thread::spawn(move || warm.ensure_started());
        handle.join().unwrap().unwrap();
        // The spawning thread has exited; PDEATHSIG delivery is
        // immediate, but allow a brief moment for it to land.
        std::thread::sleep(Duration::from_millis(300));
        let result = signer.sign(&req()).unwrap();
        assert_eq!(result.sign, "FAKESIGN");
        signer.close();
    }

    #[cfg(unix)]
    #[test]
    fn serve_signer_ready_line_validation() {
        // initRc != 0 must be refused at startup, before any sign request.
        let script = "#!/bin/sh\necho '{\"ready\": true, \"initRc\": 1, \"appKey\": \"21407387\"}'\nwhile IFS= read -r line; do echo '{\"sign\": \"X\"}'; done\n";
        let signer = ServeSigner::with_options(
            write_shell_script(script, "badrc"),
            Duration::from_secs(10),
            false,
        )
        .unwrap();
        let err = signer.sign(&req()).unwrap_err().to_string();
        assert!(err.contains("SG init failure"), "{err}");
        // permanently broken: a second call fails immediately with the same error
        let err2 = signer.sign(&req()).unwrap_err().to_string();
        assert_eq!(err, err2);
        // empty appKey also refused
        let script2 = "#!/bin/sh\necho '{\"ready\": true, \"initRc\": 0, \"appKey\": \"\"}'\nwhile IFS= read -r line; do echo '{\"sign\": \"X\"}'; done\n";
        let signer2 = ServeSigner::with_options(
            write_shell_script(script2, "badkey"),
            Duration::from_secs(10),
            false,
        )
        .unwrap();
        let err = signer2.sign(&req()).unwrap_err().to_string();
        assert!(err.contains("appKey"), "{err}");
        // harness error reply -> protocol failure, sign value never surfaces
        let script3 = "#!/bin/sh\necho '{\"ready\": true}'\nwhile IFS= read -r line; do echo '{\"error\": \"boom\"}'; done\n";
        let signer3 = ServeSigner::with_options(
            write_shell_script(script3, "errreply"),
            Duration::from_secs(10),
            false,
        )
        .unwrap();
        let err = signer3.sign(&req()).unwrap_err().to_string();
        assert!(err.contains("protocol"), "{err}");
        assert!(!err.contains("boom"));
    }

    #[cfg(unix)]
    #[test]
    fn serve_signer_startup_timeout_is_separate_from_sign_timeout() {
        // Harness that takes ~0.4s to report ready, then answers
        // sign requests immediately. A single shared 100ms budget
        // (with_options) kills the start; a split budget with a
        // generous startup allowance (with_timeouts) lets it boot —
        // the regression shape of the 300s startup timeouts.
        let script = "#!/bin/sh\nsleep 0.4\necho '{\"ready\": true, \"initRc\": 0, \"appKey\": \"21407387\"}'\nwhile IFS= read -r line; do echo '{\"sign\": \"FAKESIGN\"}'; done\n";
        let shared = ServeSigner::with_options(
            write_shell_script(script, "sharedbudget"),
            Duration::from_millis(100),
            false,
        )
        .unwrap();
        let err = shared.sign(&req()).unwrap_err().to_string();
        assert!(err.contains("startup"), "{err}");
        assert!(err.contains("timed out"), "{err}");

        let split = ServeSigner::with_timeouts(
            write_shell_script(script, "splitbudget"),
            Duration::from_millis(100),
            Duration::from_secs(5),
            false,
        )
        .unwrap();
        // Pre-warm path: starts the harness without signing, and a
        // second call is a no-op on the running harness.
        split.ensure_started().unwrap();
        assert!(split.is_started());
        split.ensure_started().unwrap();
        let result = split.sign(&req()).unwrap();
        assert_eq!(result.sign, "FAKESIGN");
        split.close();
    }

    #[cfg(unix)]
    #[test]
    fn serve_signer_ensure_started_latches_failure() {
        // A harness that never reports ready: ensure_started fails
        // within the startup budget, and the failure is latched —
        // a later sign returns the same error without restarting.
        let script = "#!/bin/sh\nsleep 30\n";
        let signer = ServeSigner::with_timeouts(
            write_shell_script(script, "neverready"),
            Duration::from_secs(5),
            Duration::from_millis(300),
            false,
        )
        .unwrap();
        let err = signer.ensure_started().unwrap_err().to_string();
        assert!(err.contains("startup"), "{err}");
        let err2 = signer.sign(&req()).unwrap_err().to_string();
        assert_eq!(err, err2);
    }

    #[cfg(target_os = "linux")]
    fn process_alive(pid: i32) -> bool {
        // Alive = /proc entry exists and is not a zombie.
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => match stat.rsplit_once(')') {
                Some((_, rest)) => !rest.trim_start().starts_with('Z'),
                None => true,
            },
            Err(_) => false,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn serve_signer_drop_kills_harness_tree() {
        // Fake harness that spawns a grandchild (sleep) and records
        // its pid: dropping the signer must kill the whole tree,
        // not just the direct child (issue #2 orphan processes).
        let name = "tree";
        let dir =
            std::env::temp_dir().join(format!("gf_serve_test_{}_{}", std::process::id(), name));
        let pid_file = dir.join("grandchild.pid");
        let body = format!(
            "#!/bin/sh\nsleep 300 &\necho $! > '{}'\necho '{{\"ready\": true, \"initRc\": 0, \"appKey\": \"21407387\"}}'\nwhile IFS= read -r line; do echo '{{\"sign\": \"FAKESIGN\"}}'; done\n",
            pid_file.display()
        );
        let signer = ServeSigner::with_options(
            write_shell_script(&body, name),
            Duration::from_secs(10),
            false,
        )
        .unwrap();
        let result = signer.sign(&req()).unwrap();
        assert_eq!(result.sign, "FAKESIGN");
        assert!(signer.is_started());
        assert!(signer.last_startup_duration().is_some());
        let grandchild: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            process_alive(grandchild),
            "grandchild should run while the signer lives"
        );
        drop(signer);
        let deadline = Instant::now() + Duration::from_secs(3);
        while process_alive(grandchild) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !process_alive(grandchild),
            "grandchild {grandchild} survived signer drop"
        );
    }

    #[cfg(unix)]
    #[test]
    fn serve_signer_restarts_dead_harness() {
        // The first harness instance answers one sign and then
        // dies mid-stream (its output closes). The signer must
        // restart it in place and serve the next sign from the new
        // instance — a dead local subprocess is transient, unlike
        // a deterministic failure.
        let name = "restart";
        let dir =
            std::env::temp_dir().join(format!("gf_serve_test_{}_{}", std::process::id(), name));
        let counter = dir.join("instances");
        let body = format!(
            "#!/bin/sh\nn=0\n[ -f '{c}' ] && n=$(cat '{c}')\nn=$((n+1))\necho $n > '{c}'\necho '{{\"ready\": true, \"initRc\": 0, \"appKey\": \"21407387\"}}'\nif [ \"$n\" -eq 1 ]; then\n  IFS= read -r line; echo '{{\"sign\": \"FIRST\"}}'\n  IFS= read -r line; exit 1\nfi\nwhile IFS= read -r line; do echo '{{\"sign\": \"SECOND\"}}'; done\n",
            c = counter.display()
        );
        let signer = ServeSigner::with_options(
            write_shell_script(&body, name),
            Duration::from_secs(10),
            false,
        )
        .unwrap();
        let first = signer.sign(&req()).unwrap();
        assert_eq!(first.sign, "FIRST");
        let second = signer.sign(&req()).unwrap();
        assert_eq!(second.sign, "SECOND");
        let third = signer.sign(&req()).unwrap();
        assert_eq!(third.sign, "SECOND");
        let instances: i32 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(instances, 2, "exactly one restart should have happened");
        signer.close();
    }

    #[cfg(unix)]
    #[test]
    fn serve_signer_restart_cap_latches() {
        // A harness that dies right after ready on every start:
        // restarts are bounded (1 initial + MAX_HARNESS_RESTARTS),
        // then the death latches and later signs fail immediately
        // without spawning further instances.
        let name = "crashloop";
        let dir =
            std::env::temp_dir().join(format!("gf_serve_test_{}_{}", std::process::id(), name));
        let counter = dir.join("instances");
        let body = format!(
            "#!/bin/sh\nn=0\n[ -f '{c}' ] && n=$(cat '{c}')\nn=$((n+1))\necho $n > '{c}'\necho '{{\"ready\": true, \"initRc\": 0, \"appKey\": \"21407387\"}}'\nexit 1\n",
            c = counter.display()
        );
        let signer = ServeSigner::with_options(
            write_shell_script(&body, name),
            Duration::from_secs(10),
            false,
        )
        .unwrap();
        let err = signer.sign(&req()).unwrap_err().to_string();
        // Which death flavour is reported depends on timing: the
        // already-exited check usually wins over the output EOF, and
        // on a fast exit the stdin write can fail first. All three
        // are HarnessDied flavours and are restarted the same way.
        assert!(
            err.contains("no longer running")
                || err.contains("closed its output")
                || err.contains("could not write"),
            "{err}"
        );
        let instances: i32 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            instances,
            1 + MAX_HARNESS_RESTARTS as i32,
            "restarts must be bounded"
        );
        let err2 = signer.sign(&req()).unwrap_err().to_string();
        assert_eq!(err, err2);
        let instances2: i32 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(instances2, instances, "latched death must not respawn");
    }
}
