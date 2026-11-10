//! sgharness: Goofish (闲鱼) SecurityGuard signing harness.
//!
//! Runs the Goofish app's own SecurityGuard native library
//! (libsgmainso) under the Unicorn engine — there is no JVM and no
//! reimplementation of the signing algorithm anywhere: the final
//! signature computation is the app's own native code, executed
//! instruction by instruction. This process only reproduces the
//! Android environment that code expects (JNI surface, syscalls,
//! thread scheduling, a staged root filesystem).
//!
//! Protocol (stdio, JSON lines), identical to the harness it
//! replaces: on startup the process loads the library, runs the
//! SecurityGuard init and prints one ready line
//!
//!   {"ready":true,"initRc":0,"appKey":"21407387"}
//!
//! then answers one request line
//!
//!   {"input": "...", "appKey": "...", "requestType": 7, "authCode": ""}
//!
//! with {"sign": "..."} (or {"error": "..."}). `appKey` may be
//! omitted, in which case the key reported at startup is used.
//! stdout carries the protocol only; every diagnostic goes to
//! stderr. Closing stdin ends the process.

// The emulation core is a faithful port of the research harness
// whose signatures were verified byte-for-byte against the JVM
// harness and live. Style-level churn is deliberately avoided so
// the code stays diffable against that verified version, so the
// style / complexity / perf / suspicious lint families and the
// dead-code notes for the port's retained scaffolding are allowed
// at crate scope. Correctness lints stay denied.
#![allow(
    clippy::style,
    clippy::complexity,
    clippy::perf,
    clippy::suspicious,
    dead_code,
    unused_imports,
    unused_parens
)]

mod jni;
mod jni_dispatch;
mod loader;
mod machine;
mod runtime;
mod syscalls;

use jni::ObjKind;
use machine::Machine;

const PKG: &str = "com.taobao.idlefish";

struct Cfg {
    rootfs: String,
    sg: String,
    syslibs: String,
    verbose_jni: bool,
}

fn parse_args() -> Result<Cfg, String> {
    let mut cfg = Cfg {
        rootfs: "rootfs".to_string(),
        sg: "assets/libsgmainso-6.7.260202.so".to_string(),
        syslibs: "assets/syslibs".to_string(),
        verbose_jni: false,
    };
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "serve" => {}
            "--rootfs" => {
                i += 1;
                cfg.rootfs = args.get(i).ok_or("--rootfs needs a value")?.clone();
            }
            "--sg" => {
                i += 1;
                cfg.sg = args.get(i).ok_or("--sg needs a value")?.clone();
            }
            "--syslibs" => {
                i += 1;
                cfg.syslibs = args.get(i).ok_or("--syslibs needs a value")?.clone();
            }
            "--verbose" => cfg.verbose_jni = true,
            "--quiet" => cfg.verbose_jni = false,
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    Ok(cfg)
}

fn describe(m: &Machine, handle: u64) -> String {
    if handle == 0 {
        return "null".to_string();
    }
    match m.jni.obj_of_handle(handle).map(|o| &o.kind) {
        Some(ObjKind::Str(s)) => format!("String(len={} value={})", s.len(), s),
        Some(ObjKind::Int(v)) => format!("Integer({})", v),
        Some(ObjKind::Long(v)) => format!("Long({})", v),
        Some(ObjKind::Bool(v)) => format!("Boolean({})", v),
        Some(ObjKind::SecException { msg, code }) => {
            format!("SecException(msg={} code={})", msg, code)
        }
        Some(k) => format!("{:?}", k),
        None => format!("handle {:#x}", handle),
    }
}

/// Minimal JSON field extraction for the serve protocol (flat objects).
fn json_get_str(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\"", key);
    let i = line.find(&pat)?;
    let rest = &line[i + pat.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    if rest.starts_with("null") {
        return None;
    }
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('u') => {
                    let h: String = chars.by_ref().take(4).collect();
                    if let Ok(n) = u32::from_str_radix(&h, 16) {
                        if let Some(ch) = char::from_u32(n) {
                            out.push(ch);
                        }
                    }
                }
                Some(other) => out.push(other),
                None => break,
            },
            _ => out.push(c),
        }
    }
    Some(out)
}

fn json_get_int(line: &str, key: &str) -> Option<i64> {
    let pat = format!("\"{}\"", key);
    let i = line.find(&pat)?;
    let rest = &line[i + pat.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let num: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    num.parse().ok()
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "usage: sgharness serve [--rootfs DIR] [--sg FILE] [--syslibs DIR] [--verbose]\n{e}"
            );
            std::process::exit(2);
        }
    };
    let rootfs = std::fs::canonicalize(&cfg.rootfs)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| cfg.rootfs.clone());
    let mut m = Machine::new(&rootfs);
    m.log_jni = cfg.verbose_jni;
    let mut uc = match runtime::new_uc(m) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[fatal] {}", e);
            std::process::exit(2);
        }
    };
    let mrc = uc.get_data().clone();
    let run = (|| -> Result<(), String> {
        runtime::setup_memory(&mut uc)?;
        {
            let mut mm = mrc.borrow_mut();
            mm.load_all(&mut uc, &cfg.sg, &cfg.syslibs)?;
        }
        {
            let mm = mrc.borrow();
            runtime::install_hooks(&mut uc, &mm)?;
        }
        {
            let mut mm = mrc.borrow_mut();
            mm.relocate_all(&mut uc)?;
        }
        // Resolve the JNI bridge class BEFORE JNI_OnLoad (the app
        // environment does the same; loading it lazily from inside
        // JNI_OnLoad deadlocks the registration).
        let jnic_class = {
            let mut mm = mrc.borrow_mut();
            mm.jni
                .class_handle("com/taobao/wireless/security/adapter/JNICLibrary")
        };
        runtime::run_inits(&mut uc)?;
        let (sg_base, onload) = {
            let mm = mrc.borrow();
            let sg = mm.modules.last().ok_or("no SG module")?;
            (
                sg.base,
                sg.sym_addr("JNI_OnLoad")
                    .ok_or("JNI_OnLoad export missing")?,
            )
        };
        eprintln!("[load] SG base={:#x} JNI_OnLoad={:#x}", sg_base, onload);
        let vm_addr = mrc.borrow().jni.vm_addr;
        let ver = runtime::call_function(&mut uc, onload, &[vm_addr, 0])?;
        eprintln!("[load] JNI_OnLoad -> {:#x}", ver);
        let native = {
            let mm = mrc.borrow();
            mm.jni
                .natives
                .iter()
                .find(|((c, n, _), _)| {
                    c == "com/taobao/wireless/security/adapter/JNICLibrary"
                        && n == "doCommandNative"
                })
                .map(|(_, &a)| a)
                .ok_or("doCommandNative not registered")?
        };
        eprintln!("[load] doCommandNative at {:#x}", native);
        // ---- helpers to build command arguments ----
        let env_addr = mrc.borrow().jni.env_addr;
        let do_command =
            |uc: &mut runtime::Uc<'static>, cmd: i32, args: &[u64]| -> Result<u64, String> {
                let arr = {
                    let mut mm = mrc.borrow_mut();
                    mm.jni.handle_of(ObjKind::ObjectArray(args.to_vec()))
                };
                eprintln!("[docommand] cmd={} argc={}", cmd, args.len());
                let r =
                    runtime::call_function(uc, native, &[env_addr, jnic_class, cmd as u64, arr])?;
                eprintln!("[docommand] cmd={} -> {}", cmd, {
                    let mm = mrc.borrow();
                    describe(&mm, r)
                });
                Ok(r)
            };
        let s_obj = |mm: &mut Machine, s: &str| mm.jni.string_obj(s);
        let i_obj = |mm: &mut Machine, v: i32| mm.jni.handle_of(ObjKind::Int(v));
        let b_obj = |mm: &mut Machine, v: bool| mm.jni.handle_of(ObjKind::Bool(v));

        // ---- init (10101) ----
        let rc_handle = {
            let args: Vec<u64> = {
                let mut mm = mrc.borrow_mut();
                let ctx = mm.jni.handle_of(ObjKind::Named {
                    class: "android/content/Context".to_string(),
                    tag: "CTX".to_string(),
                });
                let plugin = mm.jni.handle_of(ObjKind::Named {
                    class: "com/alibaba/wireless/security/mainplugin/SecurityGuardMainPlugin"
                        .to_string(),
                    tag: "PLUGIN".to_string(),
                });
                vec![
                    ctx,
                    i_obj(&mut mm, 0),
                    s_obj(&mut mm, ""),
                    s_obj(&mut mm, ""),
                    s_obj(&mut mm, ""),
                    s_obj(&mut mm, PKG),
                    s_obj(&mut mm, "7.28.70"),
                    s_obj(&mut mm, "10"),
                    s_obj(&mut mm, PKG),
                    i_obj(&mut mm, 0),
                    plugin,
                ]
            };
            do_command(&mut uc, 10101, &args)?
        };
        let rc = {
            let mm = mrc.borrow();
            match mm.jni.obj_of_handle(rc_handle).map(|o| &o.kind) {
                Some(&ObjKind::Int(v)) => v,
                _ => -999,
            }
        };
        eprintln!("[init] doCommand(10101) returned {}", rc);
        // ---- appkey (10602) ----
        let ak_handle = {
            let args: Vec<u64> = {
                let mut mm = mrc.borrow_mut();
                vec![i_obj(&mut mm, 0), s_obj(&mut mm, "")]
            };
            do_command(&mut uc, 10602, &args)?
        };
        let appkey = {
            let mm = mrc.borrow();
            mm.jni.get_string(ak_handle)
        };
        eprintln!("[appkey] getAppKeyByIndex(0) -> {:?}", appkey);
        // ---- serve loop ----
        {
            use std::io::{BufRead, Write};
            let stdout = std::io::stdout();
            {
                let mut lk = stdout.lock();
                let ak_json = match &appkey {
                    Some(a) => format!("\"{}\"", json_escape(a)),
                    None => "null".to_string(),
                };
                let _ = writeln!(
                    lk,
                    "{{\"ready\":true,\"initRc\":{},\"appKey\":{}}}",
                    rc, ak_json
                );
                let _ = lk.flush();
            }
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.trim().is_empty() {
                    continue;
                }
                let resp = (|| -> Result<String, String> {
                    let input = json_get_str(&line, "input").ok_or("missing input")?;
                    let key = json_get_str(&line, "appKey")
                        .or_else(|| appkey.clone())
                        .ok_or("no appKey")?;
                    let rt = json_get_int(&line, "requestType").unwrap_or(7) as i32;
                    let ac = json_get_str(&line, "authCode").unwrap_or_default();
                    let args: Vec<u64> = {
                        let mut mm = mrc.borrow_mut();
                        let mut map = std::collections::HashMap::new();
                        map.insert("INPUT".to_string(), input);
                        let map_obj = mm.jni.handle_of(ObjKind::HashMap(map));
                        vec![
                            map_obj,
                            s_obj(&mut mm, &key),
                            i_obj(&mut mm, rt),
                            s_obj(&mut mm, &ac),
                            b_obj(&mut mm, true),
                        ]
                    };
                    let h = do_command(&mut uc, 10401, &args)?;
                    let mm = mrc.borrow();
                    mm.jni.get_string(h).ok_or_else(|| {
                        "sign returned null (native SecException; see stderr log)".to_string()
                    })
                })();
                let mut lk = stdout.lock();
                match resp {
                    Ok(sign) => {
                        let _ = writeln!(lk, "{{\"sign\":\"{}\"}}", json_escape(&sign));
                    }
                    Err(e) => {
                        let _ = writeln!(lk, "{{\"error\":\"{}\"}}", json_escape(&e));
                    }
                }
                let _ = lk.flush();
            }
        }
        Ok(())
    })();
    if let Err(e) = run {
        eprintln!("[fatal] serve failed: {}", e);
        std::process::exit(1);
    }
}
