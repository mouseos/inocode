//! Confinement on Linux, ported from DeepSeek Harness
//! `packages/sandbox/sandbox-local/src/{index,profiles}.ts` and
//! `native/system/packages/entry/src/main.c` (landlock-run).
//!
//! `bwrap` is used when its probe passes; otherwise the runner applies a
//! Landlock ruleset to itself and executes the command, which inherits it.

use std::ffi::CString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::{Roots, RUNNER_ARGUMENT};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The mounts every bwrap run uses: the whole tree read-only, fresh `/dev`
/// and `/proc`, a separate process-ID namespace, and the sandbox ending with
/// its parent.
const BWRAP_BASE: [&str; 9] = [
    "--ro-bind",
    "/",
    "/",
    "--dev",
    "/dev",
    "--unshare-pid",
    "--proc",
    "/proc",
    "--die-with-parent",
];

pub(super) fn confine(
    argv: &[String],
    roots: &Roots,
    runner: &Path,
) -> Result<Vec<String>, String> {
    let folder = canonical(&roots.folder)?;
    let temp = canonical(&roots.temp)?;
    let mut confined = Vec::new();
    if bwrap_usable() {
        confined.push("bwrap".to_string());
        confined.extend(BWRAP_BASE.map(String::from));
        confined.extend(["--tmpfs".to_string(), "/tmp".to_string()]);
        for root in [&folder, &temp] {
            confined.extend(["--bind".to_string(), root.clone(), root.clone()]);
        }
    } else {
        let runner = runner
            .to_str()
            .ok_or_else(|| format!("the sandbox runner path {} is not UTF-8", runner.display()))?;
        confined.extend([runner.to_string(), RUNNER_ARGUMENT.to_string()]);
        for root in [&folder, &temp] {
            confined.extend(["--write".to_string(), root.clone()]);
        }
    }
    confined.push("--".to_string());
    confined.extend(argv.iter().cloned());
    Ok(confined)
}

fn canonical(path: &Path) -> Result<String, String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("could not resolve {}: {error}", path.display()))?;
    canonical
        .into_os_string()
        .into_string()
        .map_err(|path| format!("{} is not UTF-8", PathBuf::from(path).display()))
}

/// Whether `bwrap` on `PATH` can build the read-only profile, checked once
/// per process by running `true` under it.
fn bwrap_usable() -> bool {
    static USABLE: OnceLock<bool> = OnceLock::new();
    *USABLE.get_or_init(|| {
        let Ok(mut child) = Command::new("bwrap")
            .args(BWRAP_BASE)
            .args(["--", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        let deadline = Instant::now() + PROBE_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    })
}

/// The runner: `[--write <path>]... -- <argv>...`. Reads and executes are
/// allowed everywhere, every access beneath each `--write` path, and writing
/// to `/dev/null`. On success the process becomes the command, so this only
/// returns on failure.
pub(super) fn run(args: &[String]) -> Result<i32, String> {
    let (writable, command) = parse(args)?;
    let partial = landlock::restrict_self(&writable)?;
    if partial {
        eprintln!(
            "{} partial enforcement (older Landlock ABI)",
            super::RUNNER_PREFIX
        );
    }
    let error = Command::new(&command[0]).args(&command[1..]).exec();
    Err(format!("could not start {}: {error}", command[0]))
}

fn parse(args: &[String]) -> Result<(Vec<String>, &[String]), String> {
    let mut writable = vec!["/dev/null".to_string()];
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--write" => {
                let path = args
                    .get(index + 1)
                    .ok_or("usage error: --write requires a path")?;
                writable.push(path.clone());
                index += 2;
            }
            "--" => {
                let command = &args[index + 1..];
                if command.is_empty() {
                    break;
                }
                return Ok((writable, command));
            }
            other => return Err(format!("usage error: unknown argument: {other}")),
        }
    }
    Err("usage error: missing `-- <argv>...` command".into())
}

mod landlock {
    //! The raw Landlock interface (`linux/landlock.h`), negotiated down to
    //! the kernel's ABI.

    use super::CString;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }

    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: libc::c_int = 1;

    const EXECUTE: u64 = 1 << 0;
    const WRITE_FILE: u64 = 1 << 1;
    const READ_FILE: u64 = 1 << 2;
    const READ_DIR: u64 = 1 << 3;
    const REFER: u64 = 1 << 13;
    const TRUNCATE: u64 = 1 << 14;
    const IOCTL_DEV: u64 = 1 << 15;
    const ABI1_MASK: u64 = REFER - 1;
    const MAX_ABI: libc::c_long = 5;

    fn fs_mask_for_abi(abi: libc::c_long) -> u64 {
        let mut mask = ABI1_MASK;
        if abi >= 2 {
            mask |= REFER;
        }
        if abi >= 3 {
            mask |= TRUNCATE;
        }
        if abi >= 5 {
            mask |= IOCTL_DEV;
        }
        mask
    }

    fn last_error() -> String {
        std::io::Error::last_os_error().to_string()
    }

    struct Fd(libc::c_int);

    impl Drop for Fd {
        fn drop(&mut self) {
            // SAFETY: the descriptor is owned by this value and closed once.
            unsafe { libc::close(self.0) };
        }
    }

    fn add_rule(ruleset: &Fd, path: &str, access: u64) -> Result<(), String> {
        let c_path = CString::new(path).map_err(|_| format!("cannot open rule path: {path}"))?;
        // SAFETY: `c_path` is a valid NUL-terminated string.
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(format!("cannot open rule path: {path}: {}", last_error()));
        }
        let fd = Fd(fd);
        let mut access = access;
        // SAFETY: `stat` is plain data and `fd` is open.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.0, &mut stat) } == 0
            && stat.st_mode & libc::S_IFMT != libc::S_IFDIR
        {
            access &= EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;
        }
        let attr = PathBeneathAttr {
            allowed_access: access,
            parent_fd: fd.0,
        };
        // SAFETY: `attr` matches `struct landlock_path_beneath_attr`.
        let result = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset.0,
                RULE_PATH_BENEATH,
                &attr as *const PathBeneathAttr,
                0,
            )
        };
        if result != 0 {
            return Err(format!("landlock ruleset error: {}", last_error()));
        }
        Ok(())
    }

    /// Restricts the calling thread, and what it executes, to reading and
    /// executing everywhere and every access beneath `writable`. Returns
    /// whether the kernel governs only part of the accesses (an older ABI).
    pub(super) fn restrict_self(writable: &[String]) -> Result<bool, String> {
        // SAFETY: a version query takes no attribute.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0,
                CREATE_RULESET_VERSION,
            )
        };
        if abi < 0 {
            return Err("landlock is not enforced by this kernel (ABI unsupported or disabled), so the command was not run".into());
        }
        let partial = abi < MAX_ABI;
        let handled = fs_mask_for_abi(abi.min(MAX_ABI));
        let attr = RulesetAttr {
            handled_access_fs: handled,
        };
        // SAFETY: `attr` matches `struct landlock_ruleset_attr`.
        let ruleset = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0,
            )
        };
        if ruleset < 0 {
            return Err(format!("landlock ruleset error: {}", last_error()));
        }
        let ruleset = Fd(ruleset as libc::c_int);
        add_rule(&ruleset, "/", (EXECUTE | READ_FILE | READ_DIR) & handled)?;
        for path in writable {
            add_rule(&ruleset, path, handled)?;
        }
        // SAFETY: plain prctl and Landlock calls on an open ruleset.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(format!("landlock ruleset error: {}", last_error()));
        }
        if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.0, 0) } != 0 {
            return Err(format!("landlock ruleset error: {}", last_error()));
        }
        Ok(partial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELPER_ARGS: &str = "INOCODE_LANDLOCK_HELPER_ARGS";

    /// Runs [`run`] when started by [`run_confined`]; it is ignored otherwise.
    #[test]
    #[ignore]
    fn runner_helper() {
        let Ok(joined) = std::env::var(HELPER_ARGS) else {
            return;
        };
        let args: Vec<String> = joined.split('\n').map(String::from).collect();
        let error = run(&args).unwrap_err();
        eprintln!("{} {error}", super::super::RUNNER_PREFIX);
        std::process::exit(super::super::RUNNER_FAILURE);
    }

    /// Starts this test binary as the runner, as the server would start
    /// itself with `sandbox-run`.
    fn run_confined(roots: &Roots, script: &str) -> std::process::Output {
        let argv = ["sh", "-c", script].map(String::from);
        let mut args = Vec::new();
        for root in [&roots.folder, &roots.temp] {
            args.extend(["--write".to_string(), canonical(root).unwrap()]);
        }
        args.push("--".into());
        args.extend(argv);
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sandbox::linux::tests::runner_helper",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_ARGS, args.join("\n"))
            .env("TMPDIR", &roots.temp)
            .current_dir(&roots.folder)
            .output()
            .unwrap()
    }

    #[test]
    fn landlock_confines_writes_to_the_roots() {
        let base = tempfile::tempdir().unwrap();
        let roots = Roots {
            folder: base.path().join("folder"),
            temp: base.path().join("temp"),
        };
        std::fs::create_dir_all(&roots.folder).unwrap();
        std::fs::create_dir_all(&roots.temp).unwrap();
        let outside = base.path().join("outside");
        let script = format!(
            "echo a > in-folder && echo b > \"$TMPDIR/in-temp\" && echo c > /dev/null \
             && cat /etc/passwd > /dev/null; echo d > '{}'; exit 7",
            outside.display()
        );
        let output = run_confined(&roots, &script);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(7), "stderr: {stderr}");
        assert!(roots.folder.join("in-folder").is_file());
        assert!(roots.temp.join("in-temp").is_file());
        assert!(!outside.exists(), "wrote outside the roots");
        assert!(stderr.contains("Permission denied"), "stderr: {stderr}");
    }

    #[test]
    fn landlock_runner_rejects_a_missing_command() {
        let base = tempfile::tempdir().unwrap();
        let roots = Roots {
            folder: base.path().to_path_buf(),
            temp: base.path().to_path_buf(),
        };
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sandbox::linux::tests::runner_helper",
                "--ignored",
                "--nocapture",
            ])
            .env(HELPER_ARGS, "--write\n/tmp")
            .current_dir(&roots.folder)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(super::super::RUNNER_FAILURE));
    }

    #[test]
    fn confine_uses_bwrap_or_the_runner() {
        let base = tempfile::tempdir().unwrap();
        let roots = Roots {
            folder: base.path().to_path_buf(),
            temp: base.path().to_path_buf(),
        };
        let root = canonical(base.path()).unwrap();
        let argv = ["sh", "-c", "true"].map(String::from);
        let confined = confine(&argv, &roots, Path::new("/opt/inocode")).unwrap();
        let mut expected: Vec<String> = if bwrap_usable() {
            [
                "bwrap",
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--unshare-pid",
                "--proc",
                "/proc",
                "--die-with-parent",
                "--tmpfs",
                "/tmp",
                "--bind",
                &root,
                &root,
                "--bind",
                &root,
                &root,
            ]
            .map(String::from)
            .to_vec()
        } else {
            [
                "/opt/inocode",
                RUNNER_ARGUMENT,
                "--write",
                &root,
                "--write",
                &root,
            ]
            .map(String::from)
            .to_vec()
        };
        expected.push("--".into());
        expected.extend(argv);
        assert_eq!(confined, expected);
    }
}
