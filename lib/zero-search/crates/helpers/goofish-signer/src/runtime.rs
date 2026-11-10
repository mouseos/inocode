//! Driver: Unicorn setup, memory regions, hook installation, thread
//! context switching, the cooperative scheduler, and top-level guest
//! calls (init functions, JNI_OnLoad, doCommandNative).

use crate::jni;
use crate::machine::*;
use crate::syscalls;
use std::cell::RefCell;
use std::rc::Rc;
use unicorn_engine::{Prot, RegisterARM64::*, Unicorn};

pub type Uc<'a> = Unicorn<'a, Rc<RefCell<Machine>>>;

pub const SLICE: usize = 0; // 0 = run-to-block (match unidbg dispatcher semantics)

pub fn new_uc(m: Machine) -> Result<Uc<'static>, String> {
    Unicorn::new_with_data(
        unicorn_engine::Arch::ARM64,
        unicorn_engine::Mode::LITTLE_ENDIAN,
        Rc::new(RefCell::new(m)),
    )
    .map_err(|e| format!("unicorn new: {:?}", e))
}

pub fn data(uc: &Uc<'_>) -> Rc<RefCell<Machine>> {
    uc.get_data().clone()
}

/// Map fixed regions and build the JNI environment structures.
pub fn setup_memory(uc: &mut Uc<'static>) -> Result<(), String> {
    let mrc = data(uc);
    uc.mem_map(STACK_BASE, STACK_SIZE, Prot::READ | Prot::WRITE)
        .map_err(|e| format!("stack map: {:?}", e))?;
    uc.mem_map(TRAMP_BASE, TRAMP_SIZE, Prot::ALL)
        .map_err(|e| format!("tramp map: {:?}", e))?;
    uc.mem_map(SENTINEL & !0xfff, 0x2000, Prot::READ | Prot::EXEC)
        .map_err(|e| format!("sentinel map: {:?}", e))?;
    uc.mem_map(JNI_MEM_BASE, 0x10_0000, Prot::READ | Prot::WRITE)
        .map_err(|e| format!("jni mem map: {:?}", e))?;
    uc.mem_map(ARENA_BASE, ARENA_SIZE, Prot::READ | Prot::WRITE)
        .map_err(|e| format!("arena map: {:?}", e))?;
    uc.mem_map(TLS_BASE, 0x10_0000, Prot::READ | Prot::WRITE)
        .map_err(|e| format!("tls map: {:?}", e))?;
    uc.mem_map(HEAP_BRK_BASE, HEAP_BRK_SIZE, Prot::READ | Prot::WRITE)
        .map_err(|e| format!("brk map: {:?}", e))?;

    let mut m = mrc.borrow_mut();
    // Main thread stack + TLS.
    m.threads[0].sp = STACK_TOP - 0x4000;
    let tls = m.alloc_tls_block();
    m.threads[0].tpidr = tls;
    // JavaVM trampolines first, then the JNI function table.
    for _ in 0..8 {
        let a = m.alloc_tramp();
        m.jni.vm_trampolines.push(a);
    }
    for _ in 0..jni::JNI_NATIVE_TABLE_ENTRIES {
        let a = m.alloc_tramp();
        m.jni.trampolines.push(a);
    }
    // Guest-visible structs: env struct -> table; vm struct -> invoke table.
    let table_addr = JNI_MEM_BASE + 0x1000;
    let env_addr = JNI_MEM_BASE;
    let invoke_addr = JNI_MEM_BASE + 0x3000;
    let vm_addr = JNI_MEM_BASE + 0x2000;
    m.jni.env_addr = env_addr;
    m.jni.vm_addr = vm_addr;
    let tramps = m.jni.trampolines.clone();
    let vm_tramps = m.jni.vm_trampolines.clone();
    drop(m);
    uc.mem_write(env_addr, &table_addr.to_le_bytes())
        .map_err(|e| format!("env write: {:?}", e))?;
    for (i, t) in tramps.iter().enumerate() {
        uc.mem_write(table_addr + (i as u64) * 8, &t.to_le_bytes())
            .map_err(|e| format!("table write: {:?}", e))?;
    }
    uc.mem_write(vm_addr, &invoke_addr.to_le_bytes())
        .map_err(|e| format!("vm write: {:?}", e))?;
    for (i, t) in vm_tramps.iter().enumerate() {
        uc.mem_write(invoke_addr + (i as u64) * 8, &t.to_le_bytes())
            .map_err(|e| format!("invoke write: {:?}", e))?;
    }
    // TLS layout mirroring the Java oracle's measured state
    // (TlsProbe): slot0 = 0, slot1 -> pthread_internal_t block,
    // slot2 = 2, slot3 -> KernelArgumentBlock {argc, argv, envp,
    // auxv} which libc's first init function consumes and clears.
    let pthread_block = TLS_BASE + 0x8000;
    let arg_block = TLS_BASE + 0x9000;
    let argv_arr = arg_block + 0x100;
    let envp_arr = arg_block + 0x120;
    let auxv_arr = arg_block + 0x140;
    let progname = arg_block + 0x200;
    let random_bytes = arg_block + 0x240;
    let w = |uc: &mut Uc, addr: u64, v: u64| {
        uc.mem_write(addr, &v.to_le_bytes())
            .map_err(|e| format!("tls write @{:#x}: {:?}", addr, e))
    };
    w(uc, progname, 0)?; // placeholder, string written below
    uc.mem_write(progname, b"com.taobao.idlefish\0")
        .map_err(|e| format!("progname write: {:?}", e))?;
    uc.mem_write(random_bytes, &[0x5au8; 16])
        .map_err(|e| format!("random write: {:?}", e))?;
    w(uc, argv_arr, progname)?;
    w(uc, argv_arr + 8, 0)?;
    w(uc, envp_arr, 0)?;
    // auxv: AT_PAGESZ=6, AT_UID=11, AT_EUID=12, AT_GID=13, AT_EGID=14,
    // AT_HWCAP=16, AT_CLKTCK=17, AT_SECURE=23, AT_RANDOM=25, AT_NULL=0
    let auxv: [(u64, u64); 10] = [
        (6, 4096),
        (11, 10234),
        (12, 10234),
        (13, 10234),
        (14, 10234),
        (16, 0),
        (17, 100),
        (23, 0),
        (25, random_bytes),
        (0, 0),
    ];
    for (i, (t, v)) in auxv.iter().enumerate() {
        w(uc, auxv_arr + (i as u64) * 16, *t)?;
        w(uc, auxv_arr + (i as u64) * 16 + 8, *v)?;
    }
    w(uc, arg_block, 1)?; // argc
    w(uc, arg_block + 8, argv_arr)?;
    w(uc, arg_block + 16, envp_arr)?;
    w(uc, arg_block + 24, auxv_arr)?;
    w(uc, tls + 8, pthread_block)?; // slot1: pthread_internal_t*
    w(uc, tls + 16, 2)?; // slot2
    w(uc, tls + 24, arg_block)?; // slot3: argument block (consumed by libc init)
    let _ = pthread_block;
    Ok(())
}

const XREGS: [unicorn_engine::RegisterARM64; 31] = [
    X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16, X17, X18, X19, X20,
    X21, X22, X23, X24, X25, X26, X27, X28, X29, X30,
];

const QREGS: [unicorn_engine::RegisterARM64; 32] = [
    Q0, Q1, Q2, Q3, Q4, Q5, Q6, Q7, Q8, Q9, Q10, Q11, Q12, Q13, Q14, Q15, Q16, Q17, Q18, Q19, Q20,
    Q21, Q22, Q23, Q24, Q25, Q26, Q27, Q28, Q29, Q30, Q31,
];

pub fn save_ctx(uc: &Uc<'static>, m: &mut Machine, ti: usize) {
    let t = &mut m.threads[ti];
    for (i, r) in XREGS.iter().enumerate() {
        t.regs[i] = uc.reg_read(*r).unwrap_or(0);
    }
    // Vector registers matter: this libz build keeps inflate state in
    // NEON regs (d16/s19/s24) across the whole inflate_fast loop, so a
    // thread switch that drops Q0-Q31 corrupts in-flight streams.
    for (i, r) in QREGS.iter().enumerate() {
        if let Ok(b) = uc.reg_read_long(*r) {
            if b.len() == 16 {
                t.qregs[i] = [
                    u64::from_le_bytes(b[0..8].try_into().unwrap()),
                    u64::from_le_bytes(b[8..16].try_into().unwrap()),
                ];
            }
        }
    }
    t.sp = uc.reg_read(SP).unwrap_or(0);
    t.pc = uc.reg_read(PC).unwrap_or(0);
    t.tpidr = uc.reg_read(TPIDR_EL0).unwrap_or(0);
    t.nzcv = uc.reg_read(NZCV).unwrap_or(0);
}

pub fn load_ctx(uc: &mut Uc<'static>, m: &Machine, ti: usize) {
    let t = &m.threads[ti];
    for (i, r) in XREGS.iter().enumerate() {
        let _ = uc.reg_write(*r, t.regs[i]);
    }
    for (i, r) in QREGS.iter().enumerate() {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&t.qregs[i][0].to_le_bytes());
        b[8..16].copy_from_slice(&t.qregs[i][1].to_le_bytes());
        let _ = uc.reg_write_long(*r, &b);
    }
    let _ = uc.reg_write(SP, t.sp);
    let _ = uc.reg_write(TPIDR_EL0, t.tpidr);
    let _ = uc.reg_write(NZCV, t.nzcv);
}

pub fn install_hooks(uc: &mut Uc<'static>, m: &Machine) -> Result<(), String> {
    // Trampoline dispatch (JNI table, JavaVM table, import stubs).
    uc.add_code_hook(TRAMP_BASE, TRAMP_BASE + TRAMP_SIZE, |uc, addr, _size| {
        crate::jni_dispatch::dispatch_trampoline(uc, addr);
    })
    .map_err(|e| format!("tramp hook: {:?}", e))?;
    // Return sentinel.
    uc.add_code_hook(SENTINEL, SENTINEL + 0x1000, |uc, _addr, _size| {
        let mrc = uc.get_data().clone();
        let mut m = mrc.borrow_mut();
        m.stop_request = Some(StopReason::CallReturned);
        uc.emu_stop().unwrap();
    })
    .map_err(|e| format!("sentinel hook: {:?}", e))?;
    // Syscall sites.
    let sites: Vec<u64> = m.svc_sites.iter().copied().collect();
    for site in sites {
        // NOTE: Unicorn treats the hook range end as inclusive of the
        // instruction AT `end`, so a single svc site must be hooked
        // with begin == end (a site+4 range double-fires every call).
        uc.add_code_hook(site, site, |uc, _addr, _size| {
            let mrc = uc.get_data().clone();
            let mut mm = mrc.borrow_mut();
            // Re-borrow the machine mutably for the syscall handler.
            syscalls::dispatch(uc, &mut mm);
            if mm.stop_request.is_some() {
                uc.emu_stop().unwrap();
            }
        })
        .map_err(|e| format!("svc hook @{:#x}: {:?}", site, e))?;
    }
    // Invalid memory accesses: record and abort the slice.
    uc.add_mem_hook(
        unicorn_engine::unicorn_const::HookType::MEM_INVALID,
        0,
        u64::MAX,
        |uc, _mt, addr, size, _val| {
            let mrc = uc.get_data().clone();
            let mut m = mrc.borrow_mut();
            m.last_fault = Some((addr, size, false));
            false
        },
    )
    .map_err(|e| format!("mem hook: {:?}", e))?;
    Ok(())
}

fn pick_next_runnable(m: &Machine, from: usize) -> Option<usize> {
    let n = m.threads.len();
    for k in 1..=n {
        let i = (from + k) % n;
        if m.threads[i].state == ThreadState::Runnable {
            return Some(i);
        }
    }
    None
}

/// Run the cooperative scheduler until the main thread's top-level
/// call returns (PC reaches SENTINEL) or an error occurs.
pub fn run_scheduler(uc: &mut Uc<'static>) -> Result<(), String> {
    let mrc = data(uc);
    let mut idle_rounds = 0u32;
    loop {
        // Choose a runnable thread.
        let ti = {
            let m = mrc.borrow();
            if m.call_returned {
                return Ok(());
            }
            if m.threads[m.current].state == ThreadState::Runnable {
                Some(m.current)
            } else {
                pick_next_runnable(&m, m.current)
            }
        };
        let ti = match ti {
            Some(t) => t,
            None => {
                let m = mrc.borrow();
                let states: Vec<String> = m
                    .threads
                    .iter()
                    .map(|t| format!("tid {} {:?}", t.tid, t.state))
                    .collect();
                return Err(format!(
                    "deadlock: no runnable thread; states: {}",
                    states.join(", ")
                ));
            }
        };
        {
            let mut m = mrc.borrow_mut();
            m.current = ti;
            m.stop_request = None;
            load_ctx(uc, &m, ti);
        }
        let start_pc = {
            let m = mrc.borrow();
            m.threads[ti].pc
        };
        let res = uc.emu_start(start_pc, SENTINEL, 0, SLICE);
        {
            let mut m = mrc.borrow_mut();
            save_ctx(uc, &mut m, ti);
            // Main thread reaching SENTINEL = the top-level call returned.
            if m.threads[ti].pc == SENTINEL {
                if ti == 0 {
                    m.call_result_x0 = m.threads[ti].regs[0];
                    m.call_returned = true;
                    return Ok(());
                } else {
                    return Err(format!(
                        "worker tid {} returned to SENTINEL (bad LR)",
                        m.threads[ti].tid
                    ));
                }
            }
            match &res {
                Err(e) => {
                    if let Some((addr, size, _)) = m.last_fault.take() {
                        let t = &m.threads[ti];
                        let mut regs = String::new();
                        for i in 0..31 {
                            regs.push_str(&format!(" x{}={:#x}", i, t.regs[i]));
                        }
                        let mut dumps = String::new();
                        for &r in &[t.regs[0], t.regs[19], t.regs[20], t.regs[21]] {
                            if r > 0x10000 {
                                if let Ok(b) = uc.mem_read_as_vec(r, 0x60) {
                                    dumps.push_str(&format!(" mem@{:#x}={:02x?}", r, &b[..0x30]));
                                }
                            }
                        }
                        return Err(format!(
                            "memory fault: addr={:#x} size={} tid={} pc={:#x} sp={:#x}{} {}",
                            addr, size, t.tid, t.pc, t.sp, regs, dumps
                        ));
                    }
                    return Err(format!(
                        "emu error {:?} tid={} pc={:#x}",
                        e, m.threads[ti].tid, m.threads[ti].pc
                    ));
                }
                Ok(()) => {}
            }
            match m.stop_request.take() {
                Some(StopReason::CallReturned) => {
                    if ti == 0 {
                        m.call_result_x0 = m.threads[ti].regs[0];
                        m.call_returned = true;
                        return Ok(());
                    }
                    return Err("worker thread reached SENTINEL".to_string());
                }
                Some(StopReason::FutexWait { addr, val }) => {
                    let _ = val;
                    m.threads[ti].state = ThreadState::BlockedFutex(addr);
                    m.futex_waiters.entry(addr).or_default().push(ti);
                    idle_rounds = 0;
                }
                Some(StopReason::ThreadExit { code }) => {
                    let cct = m.threads[ti].clear_child_tid;
                    m.threads[ti].state = ThreadState::Exited(code);
                    if cct != 0 {
                        let _ = uc.mem_write(cct, &0u32.to_le_bytes());
                        wake_one(&mut m, cct);
                    }
                    if ti == 0 {
                        return Err(format!("main thread exited with code {}", code));
                    }
                    idle_rounds = 0;
                }
                Some(StopReason::YieldSlice) => {
                    idle_rounds = 0;
                }
                Some(StopReason::Fault { addr, note }) => {
                    return Err(format!(
                        "fault @{:#x}: {} (tid {})",
                        addr, note, m.threads[ti].tid
                    ));
                }
                None => {
                    // Slice exhausted: rotate for fairness if possible.
                    idle_rounds += 1;
                    if idle_rounds > 2000 {
                        return Err("scheduler made no progress for 2000 slices".to_string());
                    }
                    if let Some(next) = pick_next_runnable(&m, ti) {
                        if next != ti {
                            m.current = next;
                        }
                    }
                }
            }
        }
    }
}

pub fn wake_one(m: &mut Machine, addr: u64) {
    if let Some(waiters) = m.futex_waiters.get_mut(&addr) {
        for &ti in waiters.iter() {
            if matches!(m.threads[ti].state, ThreadState::BlockedFutex(_)) {
                m.threads[ti].state = ThreadState::Runnable;
                waiters.retain(|&x| x != ti);
                return;
            }
        }
    }
}

/// Call a guest function on the main thread and run until it returns.
pub fn call_function(uc: &mut Uc<'static>, addr: u64, args: &[u64]) -> Result<u64, String> {
    let mrc = data(uc);
    {
        let mut m = mrc.borrow_mut();
        m.call_returned = false;
        m.current = 0;
        let t = &mut m.threads[0];
        t.regs = [0; 31];
        for (i, a) in args.iter().enumerate() {
            t.regs[i] = *a;
        }
        t.regs[30] = SENTINEL;
        t.sp = STACK_TOP - 0x4000;
        t.pc = addr;
        t.state = ThreadState::Runnable;
    }
    run_scheduler(uc)?;
    let m = mrc.borrow();
    Ok(m.call_result_x0)
}

/// Run ifunc resolvers, DT_INIT and INIT_ARRAY for all modules in
/// load order (dependencies first, SG last).
pub fn run_inits(uc: &mut Uc<'static>) -> Result<(), String> {
    let mrc = data(uc);
    let plan: Vec<(String, Vec<u64>, Option<u64>, Vec<u64>, Vec<u64>)> = {
        let m = mrc.borrow();
        let mut plan = Vec::new();
        for md in m.modules.iter() {
            let base = md.base;
            // INIT_ARRAY/PREINIT_ARRAY entries are zero in the file and
            // materialise only through RELATIVE relocations: read them
            // back from guest memory.
            let read_arr = |addr_sz: Option<(u64, u64)>| -> Vec<u64> {
                let mut out = Vec::new();
                if let Some((va, sz)) = addr_sz {
                    let n = (sz / 8) as usize;
                    if let Ok(raw) = uc.mem_read_as_vec(base + va, n * 8) {
                        for c in raw.chunks_exact(8) {
                            out.push(u64::from_le_bytes(c.try_into().unwrap()));
                        }
                    }
                }
                out
            };
            plan.push((
                md.image.name.clone(),
                md.irelative_resolvers.clone(),
                md.image.init.map(|i| base + i),
                read_arr(md.image.init_array_addr),
                read_arr(md.image.preinit_array_addr),
            ));
        }
        plan
    };
    for (name, resolvers, init, init_array, preinit) in plan {
        for r in &resolvers {
            call_function(uc, *r, &[]).map_err(|e| format!("{} ifunc @{:#x}: {}", name, r, e))?;
        }
        for f in &preinit {
            eprintln!("[init] {} preinit {:#x}", name, f);
            call_function(uc, *f, &[0, 0, 0]).map_err(|e| format!("{} preinit: {}", name, e))?;
        }
        if let Some(f) = init {
            eprintln!("[init] {} DT_INIT {:#x}", name, f);
            call_function(uc, f, &[0, 0, 0]).map_err(|e| format!("{} DT_INIT: {}", name, e))?;
        }
        for f in &init_array {
            if *f != 0 {
                eprintln!("[init] {} init_array {:#x}", name, f);
                call_function(uc, *f, &[0, 0, 0])
                    .map_err(|e| format!("{} init_array {:#x}: {}", name, f, e))?;
            }
        }
    }
    Ok(())
}
