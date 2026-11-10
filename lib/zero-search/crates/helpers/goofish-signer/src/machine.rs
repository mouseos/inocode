//! Emulator core: memory layout, module loading/relocation, guest
//! function calls, cooperative thread scheduler, SVC/JNI/import hook
//! plumbing. All guest-visible state lives in `Machine`; the Unicorn
//! handle is passed around separately (user data = Rc<RefCell<Machine>>).

use crate::loader::{
    Module, ModuleImage, R_AARCH64_ABS64, R_AARCH64_GLOB_DAT, R_AARCH64_IRELATIVE,
    R_AARCH64_JUMP_SLOT, R_AARCH64_RELATIVE, R_AARCH64_TPREL64,
};
use std::collections::{HashMap, HashSet};
use unicorn_engine::{Prot, Unicorn};

pub const PAGE: u64 = 0x1000;

pub const LIB_BASE_START: u64 = 0x1000_0000;
pub const SG_BASE: u64 = 0x1200_0000; // same as unidbg, eases log comparison
pub const STACK_BASE: u64 = 0x7000_0000; // region grows down from STACK_TOP
pub const STACK_SIZE: u64 = 0x80_0000; // 8 MiB main stack
pub const STACK_TOP: u64 = STACK_BASE + STACK_SIZE;
pub const MMAP_BASE: u64 = 0x4000_0000;
pub const MMAP_END: u64 = 0x7000_0000;
pub const SENTINEL: u64 = 0xE000_0000; // LR target for top-level calls; hook stops there
pub const TRAMP_BASE: u64 = 0xE100_0000; // JNI + import trampoline pages
pub const TRAMP_SIZE: u64 = 0x10_0000;
pub const TLS_BASE: u64 = 0xE200_0000;
pub const JNI_MEM_BASE: u64 = 0xE400_0000; // JNIEnv/JavaVM structs + tables
pub const ARENA_BASE: u64 = 0xE500_0000; // host-owned guest strings/arrays
pub const ARENA_SIZE: u64 = 0x100_0000;
pub const HEAP_BRK_BASE: u64 = 0x5000_0000; // brk region (inside mmap space, tracked separately)
pub const HEAP_BRK_SIZE: u64 = 0x100_0000;

pub fn align_down(v: u64, a: u64) -> u64 {
    v & !(a - 1)
}
pub fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) & !(a - 1)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadState {
    Runnable,
    BlockedFutex(u64), // futex address waited on
    Exited(i32),
}

#[derive(Clone)]
pub struct Thread {
    pub tid: u32,
    pub regs: [u64; 31], // X0..X30
    pub sp: u64,
    pub pc: u64,
    pub tpidr: u64,
    pub nzcv: u64,
    pub qregs: [[u64; 2]; 32],
    pub state: ThreadState,
    pub clear_child_tid: u64,
    pub robust_list: u64,
    pub name: String,
}

impl Thread {
    pub fn new_main() -> Thread {
        Thread {
            tid: 1,
            regs: [0; 31],
            sp: 0,
            pc: 0,
            tpidr: 0,
            nzcv: 0,
            qregs: [[0; 2]; 32],
            state: ThreadState::Runnable,
            clear_child_tid: 0,
            robust_list: 0,
            name: "main".to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MmapRegion {
    pub size: u64,
    pub prot: u32,
    pub flags: u32,
    pub file: Option<(String, u64)>, // guest path + offset
}

pub struct Machine {
    pub modules: Vec<Module>,
    pub module_by_soname: HashMap<String, usize>,
    pub global_symbols: HashMap<String, u64>,
    /// svc instruction addresses discovered in loaded modules
    pub svc_sites: HashSet<u64>,
    pub threads: Vec<Thread>,
    pub current: usize, // index into threads
    pub next_tid: u32,
    pub mmap_next: u64,
    pub mmaps: HashMap<u64, MmapRegion>,
    pub brk_cur: u64,
    /// Virtual filesystem: guest path -> host path (rootfs-backed)
    pub rootfs: String,
    pub fds: HashMap<i32, crate::syscalls::FdEntry>,
    pub next_fd: i32,
    /// stop reason set by hooks for the driver loop
    pub stop_request: Option<StopReason>,
    /// top-level call completion flag (PC reached SENTINEL on main thread)
    pub call_returned: bool,
    pub call_result_x0: u64,
    pub log_jni: bool,
    pub log_syscall: bool,
    pub log_import: bool,
    /// JNI state
    pub jni: crate::jni::JniState,
    /// import trampoline: address -> symbol name (for unresolved/PLT stubs)
    pub import_stubs: HashMap<u64, String>,
    pub next_tramp: u64,
    /// futex wait queues: address -> thread indices
    pub futex_waiters: HashMap<u64, Vec<usize>>,
    /// string/array arena bump pointer
    pub arena_next: u64,
    /// TLS block bump pointer
    pub tls_next: u64,
    /// last invalid-memory fault (addr, size, write?)
    pub last_fault: Option<(u64, usize, bool)>,
    pub start_time: std::time::Instant,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StopReason {
    FutexWait { addr: u64, val: u32 },
    ThreadExit { code: i32 },
    CallReturned,
    Fault { addr: u64, note: String },
    YieldSlice,
}

impl Machine {
    pub fn new(rootfs: &str) -> Machine {
        Machine {
            modules: Vec::new(),
            module_by_soname: HashMap::new(),
            global_symbols: HashMap::new(),
            svc_sites: HashSet::new(),
            threads: vec![Thread::new_main()],
            current: 0,
            next_tid: 2,
            mmap_next: MMAP_BASE,
            mmaps: HashMap::new(),
            brk_cur: HEAP_BRK_BASE,
            rootfs: rootfs.to_string(),
            fds: HashMap::new(),
            next_fd: 3,
            stop_request: None,
            call_returned: false,
            call_result_x0: 0,
            log_jni: false,
            log_syscall: false,
            log_import: false,
            jni: crate::jni::JniState::new(),
            import_stubs: HashMap::new(),
            next_tramp: TRAMP_BASE,
            futex_waiters: HashMap::new(),
            arena_next: ARENA_BASE,
            tls_next: TLS_BASE + 0x1_0000,
            last_fault: None,
            start_time: std::time::Instant::now(),
        }
    }

    pub fn alloc_tramp(&mut self) -> u64 {
        let a = self.next_tramp;
        self.next_tramp += 16;
        a
    }

    pub fn alloc_arena(&mut self, size: u64) -> u64 {
        let a = (self.arena_next + 15) & !15;
        self.arena_next = a + size.max(1);
        a
    }

    pub fn alloc_tls_block(&mut self) -> u64 {
        let a = self.tls_next;
        self.tls_next += 0x4000;
        a
    }

    /// Load order: dependencies of the SG library first (matching a
    /// linker's breadth order closely enough for symbol resolution),
    /// then the SG library itself at SG_BASE.
    pub fn load_all(
        &mut self,
        uc: &mut Unicorn<std::rc::Rc<std::cell::RefCell<Machine>>>,
        sg_path: &str,
        syslib_dir: &str,
    ) -> Result<(), String> {
        // Fixed dependency order (DT_NEEDED closure of the SG lib).
        let order = [
            "libdl.so",
            "libc.so",
            "libc++.so",
            "libm.so",
            "libz.so",
            "liblog.so",
        ];
        let mut base = LIB_BASE_START;
        for name in order {
            let path = format!("{}/{}", syslib_dir, name);
            let data = std::fs::read(&path).map_err(|e| format!("read {}: {}", path, e))?;
            let img = ModuleImage::parse(name, data)?;
            base = self.place_module(uc, img, base)?;
        }
        let data = std::fs::read(sg_path).map_err(|e| format!("read {}: {}", sg_path, e))?;
        let img = ModuleImage::parse("libsgmainso-6.7.260202.so", data)?;
        self.place_module_at(uc, img, SG_BASE)?;
        Ok(())
    }

    fn place_module(
        &mut self,
        uc: &mut Unicorn<std::rc::Rc<std::cell::RefCell<Machine>>>,
        img: ModuleImage,
        hint_base: u64,
    ) -> Result<u64, String> {
        let base = hint_base;
        let next = self.place_module_at(uc, img, base)?;
        Ok(next)
    }

    /// Map a module at `base`, copy its segments, register exports.
    /// Returns the next free base.
    fn place_module_at(
        &mut self,
        uc: &mut Unicorn<std::rc::Rc<std::cell::RefCell<Machine>>>,
        img: ModuleImage,
        base: u64,
    ) -> Result<u64, String> {
        let span = img.span;
        uc.mem_map(base, span, Prot::ALL)
            .map_err(|e| format!("mem_map {} @{:#x}: {:?}", img.name, base, e))?;
        for seg in &img.loads {
            if seg.filesz > 0 {
                let start = (seg.offset) as usize;
                let end = start + seg.filesz as usize;
                uc.mem_write(base + seg.vaddr, &img.data[start..end])
                    .map_err(|e| format!("mem_write {} seg: {:?}", img.name, e))?;
            }
        }
        // Discover svc sites in executable segments for syscall hooking.
        for seg in &img.loads {
            if crate::loader::ModuleImage::is_exec(seg.flags) {
                let start = (seg.offset) as usize;
                let bytes = &img.data[start..start + seg.filesz as usize];
                for (i, w) in bytes.chunks_exact(4).enumerate() {
                    let insn = u32::from_le_bytes(w.try_into().unwrap());
                    if insn & 0xFFE0_001F == 0xD400_0001 {
                        self.svc_sites.insert(base + seg.vaddr + (i as u64) * 4);
                    }
                }
            }
        }
        let module = Module::new(img, base);
        for (name, addr) in &module.exports {
            self.global_symbols.entry(name.clone()).or_insert(*addr);
        }
        let soname = module.image.soname.clone();
        let idx = self.modules.len();
        self.module_by_soname.insert(soname, idx);
        eprintln!(
            "[load] {} base={:#x} span={:#x} exports={}",
            module.image.name,
            base,
            span,
            module.exports.len()
        );
        self.modules.push(module);
        Ok(base + align_up(span, 0x10_0000))
    }

    /// Apply relocations for every module. Undefined symbols resolve
    /// against the global symbol table; anything left over gets an
    /// import trampoline (host-emulated or logging stub).
    pub fn relocate_all(
        &mut self,
        uc: &mut Unicorn<std::rc::Rc<std::cell::RefCell<Machine>>>,
    ) -> Result<(), String> {
        for mi in 0..self.modules.len() {
            let (base, relocs, symbols) = {
                let m = &self.modules[mi];
                (m.base, m.image.relocs.clone(), m.image.symbols.clone())
            };
            let mut unresolved: Vec<String> = Vec::new();
            let mut irel: Vec<u64> = Vec::new();
            for r in &relocs {
                let target = base + r.offset;
                // The dynamic-linker API must be host-implemented (as in
                // unidbg): the guest libdl walks a link_map our loader does
                // not build, so the real dlsym() returns NULL for symbols
                // SG resolves at runtime (e.g. "free" inside the APK/cert
                // verification path), silently disabling that whole path.
                const HOST_DL: &[&str] =
                    &["dlsym", "dlvsym", "dlopen", "dlclose", "dlerror", "dladdr"];
                let force_host_dl = base == SG_BASE
                    && r.sym != 0
                    && HOST_DL.contains(&symbols[r.sym].name.as_str());
                let sym_addr = if r.sym != 0 {
                    let sym = &symbols[r.sym];
                    if force_host_dl {
                        let tramp = self.import_stub_for(&sym.name);
                        Some(tramp)
                    } else if sym.shndx != 0 {
                        Some(base + sym.value)
                    } else if let Some(&a) = self.global_symbols.get(&sym.name) {
                        Some(a)
                    } else if !sym.name.is_empty() {
                        // Unresolved import: give it a trampoline so the
                        // call lands in the host dispatcher (logged).
                        let tramp = self.import_stub_for(&sym.name);
                        Some(tramp)
                    } else {
                        None
                    }
                } else {
                    None
                };
                let value: u64 = match r.rtype {
                    R_AARCH64_RELATIVE => (base as i64 + r.addend) as u64,
                    R_AARCH64_GLOB_DAT | R_AARCH64_JUMP_SLOT | R_AARCH64_ABS64 => match sym_addr {
                        Some(a) => (a as i64 + r.addend) as u64,
                        None => {
                            let sym = &symbols[r.sym];
                            if !sym.name.is_empty() && !unresolved.contains(&sym.name) {
                                unresolved.push(sym.name.clone());
                            }
                            r.addend as u64
                        }
                    },
                    R_AARCH64_IRELATIVE => {
                        let resolver = (base as i64 + r.addend) as u64;
                        irel.push(resolver);
                        resolver
                    }
                    R_AARCH64_TPREL64 => {
                        // No dynamic TLS modules beyond the initial set in
                        // practice here; log and write the addend.
                        eprintln!(
                            "[load] TPREL64 reloc in {} (sym {:?}) — writing addend only",
                            self.modules[mi].image.name,
                            if r.sym != 0 {
                                symbols[r.sym].name.clone()
                            } else {
                                String::new()
                            }
                        );
                        r.addend as u64
                    }
                    other => {
                        eprintln!(
                            "[load] unhandled reloc type {} in {}",
                            other, self.modules[mi].image.name
                        );
                        continue;
                    }
                };
                uc.mem_write(target, &value.to_le_bytes())
                    .map_err(|e| format!("reloc write @{:#x}: {:?}", target, e))?;
            }
            self.modules[mi].unresolved = unresolved;
            self.modules[mi].irelative_resolvers = irel;
        }
        Ok(())
    }

    fn import_stub_for(&mut self, name: &str) -> u64 {
        // Reuse one trampoline per symbol name.
        for (addr, n) in &self.import_stubs {
            if n == name {
                return *addr;
            }
        }
        let a = self.alloc_tramp();
        self.import_stubs.insert(a, name.to_string());
        a
    }
}
