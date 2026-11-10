//! Linux (AArch64, asm-generic) syscall emulation over a rootfs-backed
//! virtual filesystem. Mirrors the behaviour the Java harness gets
//! from unidbg's ARM64SyscallHandler, including its patch: NULL or
//! unreadable pathnames yield -EFAULT instead of crashing.

use crate::machine::{Machine, StopReason, ThreadState};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use unicorn_engine::RegisterARM64::{self, *};
use unicorn_engine::Unicorn;

pub type Uc<'a> = Unicorn<'a, std::rc::Rc<std::cell::RefCell<Machine>>>;

pub enum FdKind {
    File(std::fs::File),
    Dir {
        entries: Vec<(String, u8, u64)>,
        pos: usize,
        guest_path: String,
    },
    Socket, // placeholder: network is stubbed at the Java layer in the oracle
}

pub struct FdEntry {
    pub kind: FdKind,
    pub guest_path: String,
    pub append: bool,
}

const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const EBADF: i64 = 9;
const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EEXIST: i64 = 17;
const ENOTDIR: i64 = 20;
const EINVAL: i64 = 22;
const ENOSYS: i64 = 38;
const ENOSPC: i64 = 28;

fn reg(uc: &Uc<'_>, r: RegisterARM64) -> u64 {
    uc.reg_read(r).unwrap_or(0)
}
fn set_x0(uc: &mut Uc<'_>, v: u64) {
    uc.reg_write(X0, v).unwrap();
}
fn err(code: i64) -> u64 {
    (-code) as u64
}

pub fn guest_to_host(m: &Machine, guest_path: &str) -> String {
    if guest_path.starts_with('/') {
        format!("{}{}", m.rootfs, guest_path)
    } else {
        format!("{}/{}", m.rootfs, guest_path)
    }
}

/// Read a NUL-terminated string from guest memory. Returns None for a
/// NULL pointer (callers turn that into -EFAULT like the patched
/// unidbg handler).
pub fn read_cstr(uc: &Uc<'_>, addr: u64, max: usize) -> Option<String> {
    if addr == 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut a = addr;
    while out.len() < max {
        let b = uc.mem_read_as_vec(a, 64).ok()?;
        for &c in &b {
            if c == 0 {
                return Some(String::from_utf8_lossy(&out).into_owned());
            }
            out.push(c);
        }
        a += 64;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

pub fn write_stat(uc: &mut Uc<'_>, buf: u64, meta: &std::fs::Metadata, ino: u64) {
    // asm-generic struct stat (kernel layout used by newfstatat/fstat on arm64)
    let mut b = [0u8; 128];
    let mode = {
        let ft = meta.file_type();
        let base = if ft.is_dir() {
            0o040000
        } else if ft.is_symlink() {
            0o120000
        } else {
            0o100000
        };
        base | (meta
            .permissions()
            .readonly()
            .then(|| 0o444)
            .unwrap_or(0o644))
    };
    let put = |b: &mut [u8], off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    let put32 =
        |b: &mut [u8], off: usize, v: u32| b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    put(&mut b, 0, 0xfd00); // st_dev (arbitrary)
    put(&mut b, 8, ino);
    put32(&mut b, 16, mode);
    put32(&mut b, 20, 1); // nlink
    put32(&mut b, 24, 10234); // uid
    put32(&mut b, 28, 10234); // gid
    put(&mut b, 32, 0); // rdev
    put(&mut b, 48, meta.len()); // size
    put32(&mut b, 56, 4096); // blksize
    put(&mut b, 64, meta.len() / 512); // blocks
    let t = meta
        .modified()
        .map(|m| {
            m.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        })
        .unwrap_or(0);
    put(&mut b, 72, t);
    put(&mut b, 88, t);
    put(&mut b, 104, t);
    let _ = uc.mem_write(buf, &b);
}

/// Handle one SVC from the current thread. Called from the code hook
/// with PC pointing at the svc instruction; on return, the hook has
/// advanced PC past it (we set PC ourselves for blocking calls).
pub fn dispatch(uc: &mut Uc<'_>, m: &mut Machine) {
    let nr = reg(uc, X8);
    let a0 = reg(uc, X0);
    let a1 = reg(uc, X1);
    let a2 = reg(uc, X2);
    let a3 = reg(uc, X3);
    let a4 = reg(uc, X4);
    let a5 = reg(uc, X5);
    let pc = reg(uc, PC);
    if m.log_syscall {
        eprintln!(
            "[syscall] tid={} nr={} a0={:#x} a1={:#x} a2={:#x} a3={:#x}",
            m.threads[m.current].tid, nr, a0, a1, a2, a3
        );
    }
    let ret: u64 = match nr {
        // ---- file descriptors / IO ----
        56 => sys_openat(uc, m, a0 as i32, a1, a2 as u32, a3 as u32), // openat
        57 => {
            // close
            if m.fds.remove(&(a0 as i32)).is_some() {
                0
            } else {
                err(EBADF)
            }
        }
        62 => {
            // lseek
            match m.fds.get_mut(&(a0 as i32)) {
                Some(FdEntry {
                    kind: FdKind::File(f),
                    ..
                }) => {
                    let whence = a2 as i32;
                    let pos = match whence {
                        0 => SeekFrom::Start(a1),
                        1 => SeekFrom::Current(a1 as i64),
                        2 => SeekFrom::End(a1 as i64),
                        _ => return set_x0(uc, err(EINVAL)),
                    };
                    match f.seek(pos) {
                        Ok(p) => p,
                        Err(_) => err(EINVAL),
                    }
                }
                _ => err(EBADF),
            }
        }
        63 => sys_read(uc, m, a0 as i32, a1, a2),  // read
        64 => sys_write(uc, m, a0 as i32, a1, a2), // write
        67 => {
            // pread64
            match m.fds.get_mut(&(a0 as i32)) {
                Some(FdEntry {
                    kind: FdKind::File(f),
                    ..
                }) => {
                    let cur = f.seek(SeekFrom::Current(0)).unwrap_or(0);
                    let _ = f.seek(SeekFrom::Start(a3));
                    let mut buf = vec![0u8; a2 as usize];
                    let n = f.read(&mut buf).unwrap_or(0);
                    let _ = uc.mem_write(a1, &buf[..n]);
                    let _ = f.seek(SeekFrom::Start(cur));
                    n as u64
                }
                _ => err(EBADF),
            }
        }
        25 => 0,           // fcntl: accept everything (flags are advisory here)
        29 => err(ENOSYS), // ioctl: log below via default path if needed
        79 | 80 => {
            // newfstatat / fstat
            let (path_addr, buf) = if nr == 79 { (a1, a2) } else { (0, a1) };
            if nr == 80 {
                match m.fds.get(&(a0 as i32)) {
                    Some(e) => {
                        let host = guest_to_host(m, &e.guest_path);
                        match std::fs::metadata(&host) {
                            Ok(meta) => {
                                write_stat(uc, buf, &meta, 1000 + a0 as u64);
                                0
                            }
                            Err(_) => err(ENOENT),
                        }
                    }
                    None => err(EBADF),
                }
            } else {
                match read_cstr(uc, path_addr, 4096) {
                    None => err(EFAULT),
                    Some(p) if p.is_empty() => {
                        // AT_EMPTY_PATH semantics with a valid fd
                        match m.fds.get(&(a0 as i32)) {
                            Some(e) => {
                                let host = guest_to_host(m, &e.guest_path);
                                match std::fs::metadata(&host) {
                                    Ok(meta) => {
                                        write_stat(uc, buf, &meta, 999);
                                        0
                                    }
                                    Err(_) => err(ENOENT),
                                }
                            }
                            None => err(ENOENT),
                        }
                    }
                    Some(p) => {
                        let host = guest_to_host(m, &p);
                        match std::fs::metadata(&host) {
                            Ok(meta) => {
                                write_stat(uc, buf, &meta, 4242);
                                0
                            }
                            Err(_) => err(ENOENT),
                        }
                    }
                }
            }
        }
        48 => {
            // faccessat
            match read_cstr(uc, a1, 4096) {
                None => err(EFAULT),
                Some(p) if p.is_empty() => err(ENOENT),
                Some(p) => {
                    let host = guest_to_host(m, &p);
                    if std::path::Path::new(&host).exists() {
                        0
                    } else {
                        err(ENOENT)
                    }
                }
            }
        }
        34 => {
            // mkdirat
            match read_cstr(uc, a1, 4096) {
                None => err(EFAULT),
                Some(p) => {
                    let host = guest_to_host(m, &p);
                    match std::fs::create_dir_all(&host) {
                        Ok(_) => 0,
                        Err(_) => err(EPERM),
                    }
                }
            }
        }
        35 => {
            // unlinkat
            match read_cstr(uc, a1, 4096) {
                None => err(EFAULT),
                Some(p) => {
                    let host = guest_to_host(m, &p);
                    let r = if a2 & 0x200 != 0 {
                        std::fs::remove_dir(&host)
                    } else {
                        std::fs::remove_file(&host)
                    };
                    match r {
                        Ok(_) => 0,
                        Err(_) => err(ENOENT),
                    }
                }
            }
        }
        38 => 0, // renameat: accept (storage renames are not load-bearing here)
        61 => sys_getdents64(uc, m, a0 as i32, a1, a2), // getdents64
        65 | 66 => sys_rw_vectored(uc, m, nr == 66, a0 as i32, a1, a2), // readv/writev
        78 => {
            // readlinkat: no symlinks in our rootfs view
            err(ENOENT)
        }
        43 | 44 => {
            // statfs/fstatfs: zeroed buffer, success
            let buf = if nr == 43 { a1 } else { a1 };
            let _ = uc.mem_write(buf, &[0u8; 120]);
            0
        }
        // ---- memory ----
        214 => {
            // brk
            if a0 == 0 {
                m.brk_cur
            } else if a0 >= crate::machine::HEAP_BRK_BASE
                && a0 < crate::machine::HEAP_BRK_BASE + crate::machine::HEAP_BRK_SIZE
            {
                m.brk_cur = a0;
                a0
            } else {
                m.brk_cur
            }
        }
        215 => {
            // munmap
            if let Some(_r) = m.mmaps.remove(&a0) {
                let _ = uc.mem_unmap(a0, a1);
            }
            0
        }
        216 => {
            // mremap: not expected; fail softly
            err(EINVAL)
        }
        222 => sys_mmap(uc, m, a0, a1, a2 as u32, a3 as u32, a4 as i32, a5),
        226 => {
            // mprotect
            let _ = uc.mem_protect(
                a0 & !0xfff,
                crate::machine::align_up(a1, 0x1000),
                prot_from(a2 as u32),
            );
            0
        }
        227 => 0, // msync
        // ---- process / thread ----
        172 => m.threads[m.current].tid as u64, // getpid (tid==pid for our purposes)
        178 => m.threads[m.current].tid as u64, // gettid
        174 | 175 | 176 | 177 => 10234,         // getuid/geteuid/getgid/getegid
        96 => {
            // set_tid_address
            m.threads[m.current].clear_child_tid = a0;
            m.threads[m.current].tid as u64
        }
        99 => {
            // set_robust_list
            m.threads[m.current].robust_list = a0;
            0
        }
        93 | 94 => {
            // exit / exit_group: treat both as current-thread exit for
            // now; exit_group from a worker would end everything on a
            // real kernel too (the oracle never hits it on this path).
            m.stop_request = Some(StopReason::ThreadExit { code: a0 as i32 });
            0
        }
        220 => sys_clone(uc, m, a0, a1, a2, a3, a4), // clone
        // ---- time ----
        169 => {
            // gettimeofday
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            if a0 != 0 {
                let mut b = [0u8; 16];
                b[0..8].copy_from_slice(&(now.as_secs() as i64).to_le_bytes());
                b[8..16].copy_from_slice(&(now.subsec_micros() as i64).to_le_bytes());
                let _ = uc.mem_write(a0, &b);
            }
            0
        }
        113 => {
            // clock_gettime
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            if a1 != 0 {
                let mut b = [0u8; 16];
                b[0..8].copy_from_slice(&(now.as_secs() as i64).to_le_bytes());
                b[8..16].copy_from_slice(&(now.subsec_nanos() as i64).to_le_bytes());
                let _ = uc.mem_write(a1, &b);
            }
            0
        }
        101 => 0, // nanosleep: pretend it elapsed
        124 => {
            // sched_yield: ask the driver to switch threads
            m.stop_request = Some(StopReason::YieldSlice);
            0
        }
        168 => {
            // getcpu
            if a0 != 0 {
                let _ = uc.mem_write(a0, &0u32.to_le_bytes());
            }
            if a1 != 0 {
                let _ = uc.mem_write(a1, &0u32.to_le_bytes());
            }
            0
        }
        160 => {
            // uname
            let fields = [
                "Linux",
                "localhost",
                "5.10.0-android",
                "#1 SMP PREEMPT",
                "aarch64",
                "",
            ];
            let mut b = vec![0u8; 65 * 6];
            for (i, f) in fields.iter().enumerate() {
                let fb = f.as_bytes();
                b[i * 65..i * 65 + fb.len()].copy_from_slice(fb);
            }
            let _ = uc.mem_write(a0, &b);
            0
        }
        167 => {
            // prctl
            match a0 {
                15 => {
                    // PR_SET_NAME
                    if let Some(name) = read_cstr(uc, a1, 16) {
                        m.threads[m.current].name = name;
                    }
                    0
                }
                16 => {
                    // PR_GET_NAME
                    let name = m.threads[m.current].name.clone();
                    let mut b = name.into_bytes();
                    b.push(0);
                    let _ = uc.mem_write(a1, &b);
                    0
                }
                0x5356_4d41 => 0, // PR_SET_VMA
                _ => 0,
            }
        }
        278 => {
            // getrandom
            let n = a1 as usize;
            let mut buf = vec![0u8; n];
            // Deterministic source is fine: the oracle's signature is
            // independent of randomness; use the host OS RNG anyway.
            if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
                let _ = f.read_exact(&mut buf);
            }
            let _ = uc.mem_write(a0, &buf);
            n as u64
        }
        98 => sys_futex(uc, m, a0, a1 as i32, a2, a3, a4, a5),
        198 => {
            // socket: dummy descriptor (log/stat sockets on this path)
            let fd = m.next_fd;
            m.next_fd += 1;
            m.fds.insert(
                fd,
                FdEntry {
                    kind: FdKind::Socket,
                    guest_path: format!("socket:{}:{}", a0, a1),
                    append: false,
                },
            );
            if m.log_syscall {
                eprintln!(
                    "[syscall] socket(domain={} type={:#x}) -> fd {}",
                    a0, a1, fd
                );
            }
            fd as u64
        }
        199 | 200 | 201 | 202 | 203 | 204 | 207 | 242 => {
            // socketpair/bind/listen/connect/accept/getsockname/getpeername/shutdown/sendto-ish:
            // succeed trivially for dummy sockets.
            0
        }
        206 | 212 => {
            // sendto/recvfrom on dummy sockets: report full length sent, EOF received.
            if nr == 206 { a2 } else { 0 }
        }
        233 => 0,             // madvise
        129 | 130 | 131 => 0, // kill/tkill/tgkill: no signal machinery; log-only
        132 => 0,             // sigaltstack
        134 | 135 => 0,       // rt_sigaction / rt_sigprocmask: accept and ignore
        179 => {
            // sysinfo: zeroed
            let _ = uc.mem_write(a0, &[0u8; 112]);
            0
        }
        123 => {
            // sched_getaffinity: report 4 CPUs
            let n = a2 as usize;
            let mut b = vec![0u8; n];
            if n >= 8 {
                b[0..8].copy_from_slice(&0xfu64.to_le_bytes());
            }
            let _ = uc.mem_write(a1, &b);
            0
        }
        261 => 0, // prlimit64
        73 => {
            // ppoll: fake sockets never become readable; report a clean
            // timeout (0) and clear the revents fields. Returning ENOSYS
            // here made SG's network thread exit, diverging from the
            // oracle where the thread stays alive waiting.
            if a0 != 0 && a1 > 0 && a1 < 1024 {
                for i in 0..a1 {
                    let _ = uc.mem_write(a0 + i * 8 + 6, &0u16.to_le_bytes());
                }
            }
            0
        }
        293 => err(ENOSYS), // rseq: not available; glibc/bionic tolerates ENOSYS
        _ => {
            eprintln!(
                "[syscall] UNHANDLED nr={} a0={:#x} a1={:#x} a2={:#x} from pc={:#x} (tid={})",
                nr, a0, a1, a2, pc, m.threads[m.current].tid
            );
            err(ENOSYS)
        }
    };
    set_x0(uc, ret);
    // Advance past the svc instruction (unless a stop was requested:
    // blocking/exit semantics are handled by the driver).
    if m.stop_request.is_none() {
        let _ = uc.reg_write(PC, pc + 4);
    }
}

fn prot_from(p: u32) -> unicorn_engine::Prot {
    let mut prot = unicorn_engine::Prot::NONE;
    if p & 1 != 0 {
        prot |= unicorn_engine::Prot::READ;
    }
    if p & 2 != 0 {
        prot |= unicorn_engine::Prot::WRITE;
    }
    if p & 4 != 0 {
        prot |= unicorn_engine::Prot::EXEC;
    }
    if prot == unicorn_engine::Prot::NONE {
        prot = unicorn_engine::Prot::READ;
    }
    prot
}

fn sys_openat(
    uc: &mut Uc<'_>,
    m: &mut Machine,
    dirfd: i32,
    path_addr: u64,
    flags: u32,
    _mode: u32,
) -> u64 {
    let _ = dirfd;
    let path = match read_cstr(uc, path_addr, 4096) {
        None => return err(EFAULT),
        Some(p) => p,
    };
    if path.is_empty() {
        return err(ENOENT);
    }
    // /dev/null and friends map to host equivalents inside rootfs only
    // if present; a few well-known device paths get special handling.
    if path == "/dev/null" || path == "/dev/zero" {
        let fd = m.next_fd;
        m.next_fd += 1;
        m.fds.insert(
            fd,
            FdEntry {
                kind: FdKind::File(std::fs::File::open("/dev/null").unwrap()),
                guest_path: path.clone(),
                append: false,
            },
        );
        return fd as u64;
    }
    if path == "/dev/urandom" || path == "/dev/random" {
        let fd = m.next_fd;
        m.next_fd += 1;
        m.fds.insert(
            fd,
            FdEntry {
                kind: FdKind::File(std::fs::File::open("/dev/urandom").unwrap()),
                guest_path: path.clone(),
                append: false,
            },
        );
        return fd as u64;
    }
    let host = guest_to_host(m, &path);
    if m.log_syscall {
        eprintln!("[io] openat path={} flags={:#x}", path, flags);
    }
    let is_dir = std::fs::metadata(&host)
        .map(|md| md.is_dir())
        .unwrap_or(false);
    let o_directory = flags & 0x10000 != 0;
    if is_dir || o_directory {
        match std::fs::read_dir(&host) {
            Ok(rd) => {
                let mut entries = Vec::new();
                for (i, e) in rd.flatten().enumerate() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let dt = e
                        .file_type()
                        .map(|t| if t.is_dir() { 4u8 } else { 8u8 })
                        .unwrap_or(0);
                    entries.push((name, dt, (i + 2) as u64));
                }
                let fd = m.next_fd;
                m.next_fd += 1;
                m.fds.insert(
                    fd,
                    FdEntry {
                        kind: FdKind::Dir {
                            entries,
                            pos: 0,
                            guest_path: path.clone(),
                        },
                        guest_path: path,
                        append: false,
                    },
                );
                return fd as u64;
            }
            Err(_) => return err(ENOENT),
        }
    }
    // O_EXCL with O_CREAT must fail with EEXIST when the file exists
    // (SG's lock-file protocol depends on it: sg_oc.lock & friends).
    if flags & 0x40 != 0 && flags & 0x80 != 0 && std::path::Path::new(&host).exists() {
        return err(EEXIST);
    }
    let mut oo = std::fs::OpenOptions::new();
    let accmode = flags & 3;
    oo.read(accmode != 1).write(accmode == 1 || accmode == 2);
    if flags & 0x40 != 0 {
        oo.create(true);
    }
    if flags & 0x200 != 0 {
        oo.truncate(true);
    }
    if flags & 0x400 != 0 {
        oo.append(true);
    }
    match oo.open(&host) {
        Ok(f) => {
            let fd = m.next_fd;
            m.next_fd += 1;
            if m.log_syscall {
                eprintln!("[io] -> fd {} ({})", fd, path);
            }
            m.fds.insert(
                fd,
                FdEntry {
                    kind: FdKind::File(f),
                    guest_path: path,
                    append: flags & 0x400 != 0,
                },
            );
            fd as u64
        }
        Err(e) => {
            if flags & 0x40 != 0 && e.kind() == std::io::ErrorKind::NotFound {
                // Parent may not exist yet (SG creates its dirs via
                // mkdir syscalls, but be lenient like a fresh device).
                err(ENOENT)
            } else {
                err(ENOENT)
            }
        }
    }
}

fn sys_read(uc: &mut Uc<'_>, m: &mut Machine, fd: i32, buf: u64, count: u64) -> u64 {
    if fd == 0 {
        return 0; // stdin: EOF
    }
    match m.fds.get_mut(&fd) {
        Some(FdEntry {
            kind: FdKind::Socket,
            ..
        }) => 0, // EOF
        Some(FdEntry {
            kind: FdKind::File(f),
            ..
        }) => {
            let mut b = vec![0u8; count as usize];
            match f.read(&mut b) {
                Ok(n) => {
                    let _ = uc.mem_write(buf, &b[..n]);
                    n as u64
                }
                Err(_) => err(EBADF),
            }
        }
        _ => err(EBADF),
    }
}

fn sys_write(uc: &mut Uc<'_>, m: &mut Machine, fd: i32, buf: u64, count: u64) -> u64 {
    let data = uc.mem_read_as_vec(buf, count as usize).unwrap_or_default();
    match fd {
        1 | 2 => {
            eprint!(
                "[guest:{}] {}",
                if fd == 1 { "stdout" } else { "stderr" },
                String::from_utf8_lossy(&data)
            );
            count
        }
        _ => match m.fds.get_mut(&fd) {
            Some(FdEntry {
                kind: FdKind::Socket,
                ..
            }) => count,
            Some(FdEntry {
                kind: FdKind::File(f),
                ..
            }) => match f.write(&data) {
                Ok(n) => n as u64,
                Err(_) => err(EBADF),
            },
            _ => err(EBADF),
        },
    }
}

fn sys_rw_vectored(
    uc: &mut Uc,
    m: &mut Machine,
    write: bool,
    fd: i32,
    iov: u64,
    iovcnt: u64,
) -> u64 {
    let mut total = 0u64;
    for i in 0..iovcnt {
        let ent = uc.mem_read_as_vec(iov + i * 16, 16).unwrap_or_default();
        if ent.len() < 16 {
            break;
        }
        let base = u64::from_le_bytes(ent[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(ent[8..16].try_into().unwrap());
        if len == 0 {
            continue;
        }
        let r = if write {
            sys_write(uc, m, fd, base, len)
        } else {
            sys_read(uc, m, fd, base, len)
        };
        if (r as i64) < 0 {
            return if total > 0 { total } else { r };
        }
        total += r;
        if r < len {
            break;
        }
    }
    total
}

fn sys_getdents64(uc: &mut Uc<'_>, m: &mut Machine, fd: i32, buf: u64, count: u64) -> u64 {
    let entry = match m.fds.get_mut(&fd) {
        Some(e) => e,
        None => return err(EBADF),
    };
    if let FdKind::Dir { entries, pos, .. } = &mut entry.kind {
        let mut out: Vec<u8> = Vec::new();
        while *pos < entries.len() {
            let (name, dt, ino) = &entries[*pos];
            let reclen = ((19 + name.len() + 1) + 7) & !7;
            if out.len() + reclen > count as usize && !out.is_empty() {
                break;
            }
            let mut rec = vec![0u8; reclen];
            rec[0..8].copy_from_slice(&ino.to_le_bytes());
            rec[8..16].copy_from_slice(&((*pos + 1) as i64).to_le_bytes());
            rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
            rec[18] = *dt;
            rec[19..19 + name.len()].copy_from_slice(name.as_bytes());
            out.extend_from_slice(&rec);
            *pos += 1;
        }
        let _ = uc.mem_write(buf, &out);
        out.len() as u64
    } else {
        err(ENOTDIR)
    }
}

fn sys_mmap(
    uc: &mut Uc<'_>,
    m: &mut Machine,
    addr: u64,
    len: u64,
    prot: u32,
    flags: u32,
    fd: i32,
    offset: u64,
) -> u64 {
    let len_aligned = crate::machine::align_up(len, 0x1000);
    let anon = flags & 0x20 != 0;
    let fixed = flags & 0x10 != 0;
    let base = if fixed && addr != 0 {
        addr & !0xfff
    } else if addr != 0 && m.mmaps.contains_key(&(addr & !0xfff)) {
        addr & !0xfff
    } else {
        let b = m.mmap_next;
        m.mmap_next += len_aligned;
        b
    };
    if uc.mem_map(base, len_aligned, prot_from(prot)).is_err() {
        // Region may already be mapped (MAP_FIXED over existing) — try unmap first.
        let _ = uc.mem_unmap(base, len_aligned);
        if uc.mem_map(base, len_aligned, prot_from(prot)).is_err() {
            return err(ENOSPC);
        }
    }
    let mut file_ref = None;
    if !anon && fd >= 0 {
        if let Some(e) = m.fds.get_mut(&fd) {
            if let FdKind::File(f) = &mut e.kind {
                let mut b = vec![0u8; len as usize];
                let cur = f.seek(SeekFrom::Current(0)).unwrap_or(0);
                let _ = f.seek(SeekFrom::Start(offset));
                let _ = f.read(&mut b);
                let _ = f.seek(SeekFrom::Start(cur));
                let _ = uc.mem_write(base, &b);
                file_ref = Some((e.guest_path.clone(), offset));
            }
        }
    }
    m.mmaps.insert(
        base,
        crate::machine::MmapRegion {
            size: len_aligned,
            prot,
            flags,
            file: file_ref,
        },
    );
    base
}

fn sys_clone(
    uc: &mut Uc<'_>,
    m: &mut Machine,
    flags: u64,
    stack: u64,
    ptid: u64,
    tls: u64,
    ctid: u64,
) -> u64 {
    // Emulate clone for pthread_create: snapshot the current context,
    // create a child thread that returns 0 from clone with the child
    // stack/TLS. Bionic's __bionic_clone epilogue in the child picks
    // the thread function from the child stack, so an exact register
    // snapshot (with X0=0, SP=stack, TPIDR_EL0=tls) is the faithful
    // emulation.
    let xregs = [
        X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16, X17, X18, X19,
        X20, X21, X22, X23, X24, X25, X26, X27, X28, X29, X30,
    ];
    let parent = m.threads[m.current].clone();
    let tid = m.next_tid;
    m.next_tid += 1;
    let mut child = parent.clone();
    child.tid = tid;
    child.state = ThreadState::Runnable;
    child.name = format!("thread-{}", tid);
    for (i, r) in xregs.iter().enumerate() {
        child.regs[i] = uc.reg_read(*r).unwrap_or(0);
    }
    child.regs[0] = 0; // child sees clone() == 0
    child.sp = if stack != 0 { stack } else { parent.sp };
    child.pc = reg(uc, PC) + 4; // return past the svc, like the kernel
    child.clear_child_tid = 0;
    if flags & 0x0020_0000 != 0 {
        // CLONE_CHILD_CLEARTID
        child.clear_child_tid = ctid;
    }
    if flags & 0x0008_0000 != 0 {
        // CLONE_SETTLS
        child.tpidr = tls;
    }
    // CLONE_PARENT_SETTID (0x00100000): write tid at ptid in parent space
    if flags & 0x0010_0000 != 0 && ptid != 0 {
        let _ = uc.mem_write(ptid, &(tid as i32).to_le_bytes());
    }
    // CLONE_CHILD_SETTID (0x01000000): write tid at ctid (same memory)
    if flags & 0x0100_0000 != 0 && ctid != 0 {
        let _ = uc.mem_write(ctid, &(tid as i32).to_le_bytes());
    }
    if m.log_syscall {
        eprintln!(
            "[syscall] clone flags={:#x} -> tid {} stack={:#x} tls={:#x}",
            flags, tid, child.sp, child.tpidr
        );
    }
    m.threads.push(child);
    tid as u64
}

fn sys_futex(
    uc: &mut Uc<'_>,
    m: &mut Machine,
    uaddr: u64,
    op: i32,
    val: u64,
    timeout: u64,
    uaddr2: u64,
    val3: u64,
) -> u64 {
    let _ = (timeout, uaddr2, val3);
    let cmd = op & 0x7f; // strip FUTEX_PRIVATE_FLAG (128) / CLOCK_REALTIME (256)
    let cur = match uc.mem_read_as_vec(uaddr, 4) {
        Ok(b) => u32::from_le_bytes(b.try_into().unwrap()),
        Err(_) => return err(EFAULT),
    };
    match cmd {
        0 => {
            // FUTEX_WAIT
            if cur != val as u32 {
                return err(EAGAIN);
            }
            // Block: driver switches to another thread. PC stays on the
            // svc; on wake the driver re-dispatches (value re-checked).
            m.stop_request = Some(StopReason::FutexWait {
                addr: uaddr,
                val: val as u32,
            });
            0
        }
        1 => {
            // FUTEX_WAKE: wake up to `val` waiters
            let mut woken = 0u64;
            if let Some(waiters) = m.futex_waiters.get(&uaddr).cloned() {
                let mut remaining = Vec::new();
                for ti in waiters {
                    if woken < val {
                        if let Some(t) = m.threads.get_mut(ti) {
                            if matches!(t.state, ThreadState::BlockedFutex(_)) {
                                t.state = ThreadState::Runnable;
                                woken += 1;
                            }
                        }
                    } else {
                        remaining.push(ti);
                    }
                }
                m.futex_waiters.insert(uaddr, remaining);
            }
            woken
        }
        9 | 10 => {
            // WAIT_BITSET / WAKE_BITSET: treat like WAIT/WAKE for our purposes
            if cmd == 9 {
                if cur != val as u32 {
                    return err(EAGAIN);
                }
                m.stop_request = Some(StopReason::FutexWait {
                    addr: uaddr,
                    val: val as u32,
                });
                0
            } else {
                sys_futex(uc, m, uaddr, 1, val, 0, 0, 0)
            }
        }
        _ => {
            eprintln!("[syscall] futex op {} unhandled (uaddr={:#x})", cmd, uaddr);
            err(ENOSYS)
        }
    }
}
