//! Trampoline dispatch: JNI function table, JavaVM invoke table and
//! unresolved-import stubs. All entries return by jumping to X30 with
//! the result in X0 (or S0/D0 for float/double method results).

use crate::jni::{JniState, MethodId, ObjKind, idx};
use crate::machine::{Machine, StopReason};
use crate::runtime::Uc;
use crate::syscalls::read_cstr;
use unicorn_engine::RegisterARM64::*;

#[derive(Clone, Debug)]
pub enum Ret {
    Void,
    Obj(u64), // handle (0 = null)
    Bool(bool),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
}

impl Ret {
    pub fn as_obj(&self) -> u64 {
        match self {
            Ret::Obj(h) => *h,
            _ => 0,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Arg {
    G(u64),
    F(f32),
    D(f64),
}

impl Arg {
    pub fn g(&self) -> u64 {
        match self {
            Arg::G(v) => *v,
            Arg::F(f) => f.to_bits() as u64,
            Arg::D(d) => d.to_bits(),
        }
    }
    pub fn s(&self, jni: &JniState) -> Option<String> {
        jni.get_string(self.g())
    }
    pub fn i(&self) -> i32 {
        self.g() as i32
    }
}

fn gx(uc: &Uc<'_>, r: unicorn_engine::RegisterARM64) -> u64 {
    uc.reg_read(r).unwrap_or(0)
}

/// AAPCS64 va_list reader over guest memory.
struct VaList {
    stack: u64,
    gr_top: u64,
    vr_top: u64,
    gr_offs: i32,
    vr_offs: i32,
    addr: u64,
}

impl VaList {
    fn load(uc: &Uc<'_>, addr: u64) -> VaList {
        let b = uc.mem_read_as_vec(addr, 32).unwrap_or_else(|_| vec![0; 32]);
        let rd = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let ri = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        VaList {
            stack: rd(0),
            gr_top: rd(8),
            vr_top: rd(16),
            gr_offs: ri(24),
            vr_offs: ri(28),
            addr,
        }
    }
    fn store(&self, uc: &mut Uc<'_>) {
        let mut b = [0u8; 32];
        b[0..8].copy_from_slice(&self.stack.to_le_bytes());
        b[8..16].copy_from_slice(&self.gr_top.to_le_bytes());
        b[16..24].copy_from_slice(&self.vr_top.to_le_bytes());
        b[24..28].copy_from_slice(&self.gr_offs.to_le_bytes());
        b[28..32].copy_from_slice(&self.vr_offs.to_le_bytes());
        let _ = uc.mem_write(self.addr, &b);
    }
    fn next_gp(&mut self, uc: &Uc<'_>) -> u64 {
        let a = if self.gr_offs < 0 {
            let a = (self.gr_top as i64 + self.gr_offs as i64) as u64;
            self.gr_offs += 8;
            a
        } else {
            let a = self.stack;
            self.stack += 8;
            a
        };
        uc.mem_read_as_vec(a, 8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
            .unwrap_or(0)
    }
    fn next_fp(&mut self, uc: &Uc<'_>) -> u64 {
        let a = if self.vr_offs < 0 {
            let a = (self.vr_top as i64 + self.vr_offs as i64) as u64;
            self.vr_offs += 16;
            a
        } else {
            let a = (self.stack + 7) & !7;
            self.stack = a + 8;
            a
        };
        uc.mem_read_as_vec(a, 8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum P {
    Bool,
    Byte,
    Char,
    Short,
    Int,
    Long,
    Float,
    Double,
    Obj,
    Void,
}

fn parse_params(sig: &str) -> (Vec<P>, P) {
    let b = sig.as_bytes();
    let mut i = 0;
    let mut params = Vec::new();
    let mut ret = P::Void;
    // skip '('
    if i < b.len() && b[i] == b'(' {
        i += 1;
        while i < b.len() && b[i] != b')' {
            let (p, ni) = parse_one(b, i);
            params.push(p);
            i = ni;
        }
        i += 1; // ')'
        if i < b.len() {
            let (p, _) = parse_one(b, i);
            ret = p;
        }
    }
    (params, ret)
}

fn parse_one(b: &[u8], mut i: usize) -> (P, usize) {
    match b[i] {
        b'Z' => (P::Bool, i + 1),
        b'B' => (P::Byte, i + 1),
        b'C' => (P::Char, i + 1),
        b'S' => (P::Short, i + 1),
        b'I' => (P::Int, i + 1),
        b'J' => (P::Long, i + 1),
        b'F' => (P::Float, i + 1),
        b'D' => (P::Double, i + 1),
        b'V' => (P::Void, i + 1),
        b'L' => {
            while i < b.len() && b[i] != b';' {
                i += 1;
            }
            (P::Obj, i + 1)
        }
        b'[' => {
            // array: skip element type
            let (_, ni) = parse_one(b, i + 1);
            (P::Obj, ni)
        }
        _ => (P::Obj, i + 1),
    }
}

pub fn dispatch_trampoline(uc: &mut Uc<'_>, addr: u64) {
    let mrc = uc.get_data().clone();
    // Classify the trampoline without holding the borrow during work.
    enum Kind {
        Jni(usize),
        Vm(usize),
        Import(String),
        Unknown,
    }
    let kind = {
        let m = mrc.borrow();
        if let Some(i) = m.jni.trampolines.iter().position(|&t| t == addr) {
            Kind::Jni(i)
        } else if let Some(i) = m.jni.vm_trampolines.iter().position(|&t| t == addr) {
            Kind::Vm(i)
        } else if let Some(name) = m.import_stubs.get(&addr) {
            Kind::Import(name.clone())
        } else {
            Kind::Unknown
        }
    };
    let lr = gx(uc, X30);
    match kind {
        Kind::Jni(i) => {
            let r = dispatch_jni(uc, &mrc, i);
            write_ret(uc, &r);
        }
        Kind::Vm(i) => {
            let r = dispatch_vm(uc, &mrc, i);
            write_ret(uc, &r);
        }
        Kind::Import(name) => {
            let r = dispatch_import(uc, &mrc, &name);
            write_ret(uc, &r);
        }
        Kind::Unknown => {
            eprintln!("[tramp] unknown trampoline @{:#x} lr={:#x}", addr, lr);
            let _ = uc.reg_write(X0, 0);
        }
    }
    // Return to caller (unless a stop was requested, e.g. exit()).
    let stop = mrc.borrow().stop_request.is_some();
    if stop {
        let _ = uc.emu_stop();
    } else {
        let _ = uc.reg_write(PC, lr);
    }
}

fn write_ret(uc: &mut Uc<'_>, r: &Ret) {
    match r {
        Ret::Void => {}
        Ret::Obj(h) => {
            let _ = uc.reg_write(X0, *h);
        }
        Ret::Bool(b) => {
            let _ = uc.reg_write(X0, *b as u64);
        }
        Ret::Int(v) => {
            let _ = uc.reg_write(X0, *v as i64 as u64);
        }
        Ret::Long(v) => {
            let _ = uc.reg_write(X0, *v as u64);
        }
        Ret::Float(f) => {
            let _ = uc.reg_write(X0, f.to_bits() as u64);
            let _ = uc.reg_write(S0, f.to_bits() as u64);
        }
        Ret::Double(d) => {
            let _ = uc.reg_write(X0, d.to_bits());
            let _ = uc.reg_write(D0, d.to_bits());
        }
    }
}

fn dispatch_vm(uc: &mut Uc<'_>, mrc: &std::rc::Rc<std::cell::RefCell<Machine>>, i: usize) -> Ret {
    let env_addr = mrc.borrow().jni.env_addr;
    match i {
        6 => {
            // GetEnv(vm, &env, version)
            let out = gx(uc, X1);
            let _ = uc.mem_write(out, &env_addr.to_le_bytes());
            Ret::Int(0)
        }
        4 | 7 => {
            // AttachCurrentThread / AsDaemon(vm, &env, args)
            let out = gx(uc, X1);
            let _ = uc.mem_write(out, &env_addr.to_le_bytes());
            Ret::Int(0)
        }
        3 | 5 => Ret::Int(0), // DestroyJavaVM / DetachCurrentThread
        _ => {
            eprintln!("[jvm] unhandled invoke fn {}", i);
            Ret::Int(0)
        }
    }
}

fn dispatch_import(
    uc: &mut Uc<'_>,
    mrc: &std::rc::Rc<std::cell::RefCell<Machine>>,
    name: &str,
) -> Ret {
    let mut m = mrc.borrow_mut();
    if m.log_import {
        eprintln!("[import] {} (x0={:#x})", name, gx(uc, X0));
    }
    match name {
        "exit" | "_exit" | "_Exit" => {
            m.stop_request = Some(StopReason::ThreadExit {
                code: gx(uc, X0) as i32,
            });
            let _ = uc.emu_stop();
            Ret::Void
        }
        "abort" => {
            m.stop_request = Some(StopReason::ThreadExit { code: -6 });
            let _ = uc.emu_stop();
            Ret::Void
        }
        "__stack_chk_fail" => {
            m.stop_request = Some(StopReason::Fault {
                addr: 0,
                note: "stack_chk_fail via import stub".to_string(),
            });
            let _ = uc.emu_stop();
            Ret::Void
        }
        "dlsym" | "dlvsym" => {
            let symname = crate::syscalls::read_cstr(uc, gx(uc, X1), 256).unwrap_or_default();
            let addr = m.global_symbols.get(&symname).copied().unwrap_or(0);
            eprintln!("[import] {}({:?}) -> {:#x}", name, symname, addr);
            Ret::Long(addr as i64)
        }
        "dlopen" => {
            let path = crate::syscalls::read_cstr(uc, gx(uc, X0), 512).unwrap_or_default();
            let mut handle = 0u64;
            if path.is_empty() {
                handle = crate::machine::SG_BASE;
            } else {
                for mo in &m.modules {
                    if path.ends_with(&mo.image.name)
                        || mo
                            .image
                            .name
                            .ends_with(path.rsplit('/').next().unwrap_or(""))
                    {
                        handle = mo.base;
                        break;
                    }
                }
                if handle == 0 && path.contains("sgmain") {
                    handle = crate::machine::SG_BASE;
                }
            }
            eprintln!("[import] dlopen({:?}) -> {:#x}", path, handle);
            Ret::Long(handle as i64)
        }
        "dlclose" => Ret::Int(0),
        "dlerror" => Ret::Int(0),
        "dladdr" => {
            // Dl_info { fname, fbase, sname, saddr }: report module base.
            let addr = gx(uc, X0);
            let info = gx(uc, X1);
            let mut fbase = 0u64;
            for mo in &m.modules {
                if addr >= mo.base && addr < mo.base + 0x400000 {
                    fbase = mo.base;
                    break;
                }
            }
            if info != 0 {
                let _ = uc.mem_write(info, &[0u8; 32]);
                let _ = uc.mem_write(info + 8, &fbase.to_le_bytes());
            }
            Ret::Int(1)
        }
        _ => Ret::Int(0),
    }
}

// ---------- JNI function table ----------

fn obj_class_name(jni: &JniState, handle: u64) -> String {
    match jni.obj_of_handle(handle).map(|o| &o.kind) {
        Some(ObjKind::Class { .. }) => "java/lang/Class".to_string(),
        Some(ObjKind::Str(_)) => "java/lang/String".to_string(),
        Some(ObjKind::Int(_)) => "java/lang/Integer".to_string(),
        Some(ObjKind::Long(_)) => "java/lang/Long".to_string(),
        Some(ObjKind::Bool(_)) => "java/lang/Boolean".to_string(),
        Some(ObjKind::Float(_)) => "java/lang/Float".to_string(),
        Some(ObjKind::Double(_)) => "java/lang/Double".to_string(),
        Some(ObjKind::ByteArray(_)) => "[B".to_string(),
        Some(ObjKind::PrimArray { elem, .. }) => match elem {
            1 => "[Z".to_string(),
            2 => "[C".to_string(),
            4 => "[I".to_string(),
            8 => "[J".to_string(),
            _ => "[B".to_string(),
        },
        Some(ObjKind::ObjectArray(_)) => "[Ljava/lang/Object;".to_string(),
        Some(ObjKind::HashMap(_)) => "java/util/HashMap".to_string(),
        Some(ObjKind::SecException { .. }) => {
            "com/taobao/wireless/security/sdk/SecException".to_string()
        }
        Some(ObjKind::Named { class, .. }) => class.clone(),
        _ => "java/lang/Object".to_string(),
    }
}

/// Collect call arguments for the params of `sig` from one of the
/// three JNI call forms. `form`: 0=variadic regs (base index of first
/// variadic GP reg), 1=va_list at reg, 2=jvalue array at reg.
fn collect_args(
    uc: &mut Uc<'_>,
    sig: &str,
    form: u8,
    src_reg: unicorn_engine::RegisterARM64,
) -> Vec<Arg> {
    let (params, _) = parse_params(sig);
    let mut out = Vec::new();
    match form {
        1 => {
            let va_addr = gx(uc, src_reg);
            let mut va = VaList::load(uc, va_addr);
            for p in &params {
                match p {
                    P::Float => out.push(Arg::F(f32::from_bits(va.next_gp(uc) as u32))),
                    P::Double => out.push(Arg::D(f64::from_bits(va.next_gp(uc)))),
                    _ => out.push(Arg::G(va.next_gp(uc))),
                }
            }
            // Note: AAPCS FP args to variadic JNI V-calls arrive via the
            // register save area modelled by VaList's vr path when the
            // caller used va_start in compiled code; the gp path above
            // is what clang emits for JNI's own prototypes (doubles are
            // passed in GP regs for C variadic promotion in the JNI
            // Call*MethodV wrappers used by SG). If FP returns look
            // wrong, revisit here.
            va.store(uc);
        }
        2 => {
            let arr = gx(uc, src_reg);
            for (i, p) in params.iter().enumerate() {
                let v = uc
                    .mem_read_as_vec(arr + (i as u64) * 8, 8)
                    .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
                    .unwrap_or(0);
                match p {
                    P::Float => out.push(Arg::F(f32::from_bits(v as u32))),
                    P::Double => out.push(Arg::D(f64::from_bits(v))),
                    _ => out.push(Arg::G(v)),
                }
            }
        }
        _ => {
            // Variadic: GP args from the first variadic register on,
            // FP args unsupported (logged).
            let regs: &[unicorn_engine::RegisterARM64] = if src_reg == X4 {
                &[X4, X5, X6, X7]
            } else {
                &[X3, X4, X5, X6, X7]
            };
            let sp = gx(uc, SP);
            let mut gi = 0usize;
            let mut stack_i = 0u64;
            for p in &params {
                match p {
                    P::Float | P::Double => {
                        eprintln!("[jni] variadic FP arg not read from Q regs (returning 0)");
                        out.push(if *p == P::Float {
                            Arg::F(0.0)
                        } else {
                            Arg::D(0.0)
                        });
                    }
                    _ => {
                        if gi < regs.len() {
                            out.push(Arg::G(gx(uc, regs[gi])));
                            gi += 1;
                        } else {
                            let v = uc
                                .mem_read_as_vec(sp + stack_i * 8, 8)
                                .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
                                .unwrap_or(0);
                            stack_i += 1;
                            out.push(Arg::G(v));
                        }
                    }
                }
            }
        }
    }
    out
}

fn dispatch_jni(uc: &mut Uc<'_>, mrc: &std::rc::Rc<std::cell::RefCell<Machine>>, i: usize) -> Ret {
    let x1 = gx(uc, X1);
    let x2 = gx(uc, X2);
    let x3 = gx(uc, X3);
    // Method-call families handled by ranges first.
    // Virtual calls 34..=63, nonvirtual 64..=93, static 114..=143.
    let family = if (34..=63).contains(&i) {
        Some((i - 34, false, false))
    } else if (64..=93).contains(&i) {
        Some((i - 64, false, true))
    } else if (114..=143).contains(&i) {
        Some((i - 114, true, false))
    } else {
        None
    };
    if let Some((off, is_static, nonvirtual)) = family {
        let form = (off % 3) as u8;
        let (obj, mid_id, arg_reg) = if is_static {
            (x1, x2, X3)
        } else if nonvirtual {
            (x1, x3, X4)
        } else {
            (x1, x2, X3)
        };
        let mid = {
            let m = mrc.borrow();
            m.jni.method_by_id.get(&mid_id).cloned()
        };
        let mid = match mid {
            Some(m2) => m2,
            None => {
                eprintln!("[jni] call with unknown method id {:#x} (fn {})", mid_id, i);
                return Ret::Int(0);
            }
        };
        let args = collect_args(uc, &mid.sig, form, arg_reg);
        let mut m = mrc.borrow_mut();
        if m.log_jni {
            eprintln!(
                "[jni] {}.{} {} obj={:#x} args={:?} static={}",
                mid.class, mid.name, mid.sig, obj, args, is_static
            );
        }
        return call_java(uc, &mut m, &mid, obj, &args, is_static);
    }
    // Field getters 95..=103, setters 104..=112, static get 145..=153, set 154..=162.
    if (95..=112).contains(&i) || (145..=162).contains(&i) {
        let is_static = i >= 145;
        let local = if is_static { i - 145 } else { i - 95 };
        let is_set = local >= 9;
        let fid_id = x2;
        let fid = {
            let m = mrc.borrow();
            m.jni.field_by_id.get(&fid_id).cloned()
        };
        let fid = match fid {
            Some(f) => f,
            None => {
                eprintln!(
                    "[jni] field access with unknown field id {:#x} (fn {})",
                    fid_id, i
                );
                return Ret::Int(0);
            }
        };
        let mut m = mrc.borrow_mut();
        if is_set {
            let val = Arg::G(x3);
            set_java_field(&mut m, &fid, x1, &val, is_static);
            return Ret::Void;
        }
        if m.log_jni {
            eprintln!(
                "[jni] getfield {}.{} {} static={}",
                fid.class, fid.name, fid.sig, is_static
            );
        }
        return get_java_field(uc, &mut m, &fid, x1, is_static);
    }

    let mut m = mrc.borrow_mut();
    match i {
        idx::GET_VERSION => Ret::Int(0x0001_0006),
        idx::FIND_CLASS => {
            let name = read_cstr(uc, x1, 512).unwrap_or_default();
            if m.log_jni {
                eprintln!("[jni] FindClass {}", name);
            }
            Ret::Obj(m.jni.class_handle(&name))
        }
        5 => {
            // DefineClass
            let name = read_cstr(uc, x1, 512).unwrap_or_default();
            Ret::Obj(m.jni.class_handle(&name))
        }
        idx::GET_SUPERCLASS => {
            let _name = m.jni.class_name_of_handle(x1).unwrap_or_default();
            Ret::Obj(m.jni.class_handle("java/lang/Object"))
        }
        idx::IS_ASSIGNABLE_FROM => {
            let a = m.jni.class_name_of_handle(x1).unwrap_or_default();
            let b = m.jni.class_name_of_handle(x2).unwrap_or_default();
            Ret::Bool(a == b || b == "java/lang/Object")
        }
        idx::THROW_NEW => {
            let class = m.jni.class_name_of_handle(x1).unwrap_or_default();
            let msg = read_cstr(uc, x2, 1024).unwrap_or_default();
            let id = m.jni.alloc_obj(ObjKind::Named { class, tag: msg });
            m.jni.pending_exception = Some(id);
            Ret::Int(0)
        }
        13 => {
            // Throw
            if let Some(id) = m.jni.obj_id_of_handle(x1) {
                m.jni.pending_exception = Some(id);
            }
            Ret::Int(0)
        }
        idx::EXCEPTION_OCCURRED => match m.jni.pending_exception {
            Some(id) => Ret::Obj(m.jni.new_handle(id)),
            None => Ret::Obj(0),
        },
        idx::EXCEPTION_CLEAR => {
            m.jni.pending_exception = None;
            Ret::Void
        }
        idx::EXCEPTION_CHECK => Ret::Bool(m.jni.pending_exception.is_some()),
        idx::NEW_GLOBAL_REF | idx::NEW_LOCAL_REF | 226 => {
            // NewGlobalRef / NewLocalRef / NewWeakGlobalRef
            if x1 == 0 {
                Ret::Obj(0)
            } else if let Some(id) = m.jni.obj_id_of_handle(x1) {
                Ret::Obj(m.jni.new_handle(id))
            } else {
                Ret::Obj(0)
            }
        }
        idx::DELETE_GLOBAL_REF | idx::DELETE_LOCAL_REF | 227 => {
            m.jni.cells.remove(&x1);
            Ret::Void
        }
        idx::IS_SAME_OBJECT => {
            let a = m.jni.obj_id_of_handle(x1);
            let b = m.jni.obj_id_of_handle(x2);
            Ret::Bool(
                a == b
                    || (x1 == 0 && x2 == 0)
                    || (x1 == 0 && b.is_none())
                    || (x2 == 0 && a.is_none()),
            )
        }
        26 => Ret::Int(0),  // EnsureLocalCapacity
        19 => Ret::Int(0),  // PushLocalFrame
        20 => Ret::Obj(x1), // PopLocalFrame(result)
        idx::ALLOC_OBJECT => {
            let class = m.jni.class_name_of_handle(x1).unwrap_or_default();
            Ret::Obj(m.jni.handle_of(ObjKind::Named {
                class,
                tag: String::new(),
            }))
        }
        idx::NEW_OBJECT | idx::NEW_OBJECT_V | idx::NEW_OBJECT_A => {
            let class = m.jni.class_name_of_handle(x1).unwrap_or_default();
            let mid = m.jni.method_by_id.get(&x2).cloned();
            let sig = mid.map(|mm| mm.sig).unwrap_or_else(|| "()V".to_string());
            let form = (i - idx::NEW_OBJECT) as u8;
            drop(m);
            let args = collect_args(uc, &sig, form, X3);
            let mut m = mrc.borrow_mut();
            if m.log_jni {
                eprintln!("[jni] NewObject {} {}", class, sig);
            }
            Ret::Obj(construct_object(&mut m, &class, &sig, &args))
        }
        idx::GET_OBJECT_CLASS => {
            let name = obj_class_name(&m.jni, x1);
            Ret::Obj(m.jni.class_handle(&name))
        }
        idx::IS_INSTANCE_OF => {
            let a = obj_class_name(&m.jni, x1);
            let b = m.jni.class_name_of_handle(x2).unwrap_or_default();
            Ret::Bool(a == b || b == "java/lang/Object")
        }
        idx::GET_METHOD_ID | idx::GET_STATIC_METHOD_ID => {
            let class = m.jni.class_name_of_handle(x1).unwrap_or_default();
            let name = read_cstr(uc, x2, 256).unwrap_or_default();
            let sig = read_cstr(uc, x3, 512).unwrap_or_default();
            let id = m
                .jni
                .method_id(&class, &name, &sig, i == idx::GET_STATIC_METHOD_ID);
            if m.log_jni {
                eprintln!(
                    "[jni] {} {}.{} {}",
                    if i == idx::GET_STATIC_METHOD_ID {
                        "GetStaticMethodID"
                    } else {
                        "GetMethodID"
                    },
                    class,
                    name,
                    sig
                );
            }
            Ret::Long(id as i64)
        }
        idx::GET_FIELD_ID | idx::GET_STATIC_FIELD_ID => {
            let class = m.jni.class_name_of_handle(x1).unwrap_or_default();
            let name = read_cstr(uc, x2, 256).unwrap_or_default();
            let sig = read_cstr(uc, x3, 512).unwrap_or_default();
            let id = m
                .jni
                .field_id(&class, &name, &sig, i == idx::GET_STATIC_FIELD_ID);
            if m.log_jni {
                eprintln!("[jni] GetFieldID {}.{} {}", class, name, sig);
            }
            Ret::Long(id as i64)
        }
        idx::NEW_STRING_UTF => {
            let s = read_cstr(uc, x1, 1 << 20).unwrap_or_default();
            Ret::Obj(m.jni.string_obj(&s))
        }
        idx::GET_STRING_UTF_LENGTH => {
            let s = m.jni.get_string(x1).unwrap_or_default();
            Ret::Int(s.len() as i32)
        }
        idx::GET_STRING_UTF_CHARS => {
            let oid = m.jni.obj_id_of_handle(x1);
            if let Some(oid) = oid {
                if let Some(&cached) = m.jni.utf_cache.get(&oid) {
                    if x2 != 0 {
                        let _ = uc.mem_write(x2, &[0u8; 1]);
                    }
                    return Ret::Obj(cached);
                }
            }
            let s = string_content(&m, x1);
            let addr = m.alloc_arena(s.len() as u64 + 1);
            let mut b = s.into_bytes();
            b.push(0);
            let _ = uc.mem_write(addr, &b);
            if let Some(oid) = oid {
                m.jni.utf_cache.insert(oid, addr);
            }
            if x2 != 0 {
                let _ = uc.mem_write(x2, &[0u8; 1]);
            }
            Ret::Obj(addr)
        }
        idx::RELEASE_STRING_UTF_CHARS => Ret::Void,
        idx::NEW_STRING => {
            let len = x2 as usize;
            let raw = uc.mem_read_as_vec(x1, len * 2).unwrap_or_default();
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes(c.try_into().unwrap()))
                .collect();
            let s = String::from_utf16_lossy(&units);
            Ret::Obj(m.jni.string_obj(&s))
        }
        idx::GET_STRING_LENGTH => {
            let s = m.jni.get_string(x1).unwrap_or_default();
            Ret::Int(s.chars().count() as i32)
        }
        idx::GET_STRING_CHARS => {
            let s = string_content(&m, x1);
            let addr = m.alloc_arena((s.len() as u64 + 1) * 2);
            let mut b = Vec::new();
            for u in s.encode_utf16() {
                b.extend_from_slice(&u.to_le_bytes());
            }
            b.extend_from_slice(&[0, 0]);
            let _ = uc.mem_write(addr, &b);
            Ret::Obj(addr)
        }
        idx::RELEASE_STRING_CHARS => Ret::Void,
        idx::GET_STRING_UTF_REGION => {
            let s = m.jni.get_string(x1).unwrap_or_default();
            let chars: Vec<char> = s.chars().collect();
            let start = x2 as usize;
            let len = x3 as usize;
            let buf = gx(uc, X4);
            let sub: String = chars.iter().skip(start).take(len).collect();
            let _ = uc.mem_write(buf, sub.as_bytes());
            Ret::Void
        }
        220 => {
            // GetStringRegion (UTF-16)
            let s = m.jni.get_string(x1).unwrap_or_default();
            let units: Vec<u16> = s.encode_utf16().collect();
            let start = x2 as usize;
            let len = x3 as usize;
            let buf = gx(uc, X4);
            let mut b = Vec::new();
            for u in units.iter().skip(start).take(len) {
                b.extend_from_slice(&u.to_le_bytes());
            }
            let _ = uc.mem_write(buf, &b);
            Ret::Void
        }
        idx::GET_ARRAY_LENGTH => {
            let n = match m.jni.obj_of_handle(x1).map(|o| &o.kind) {
                Some(ObjKind::ByteArray(v)) => v.len(),
                Some(ObjKind::PrimArray { elem, data }) => data.len() / (*elem as usize).max(1),
                Some(ObjKind::ObjectArray(v)) => v.len(),
                _ => 0,
            };
            Ret::Int(n as i32)
        }
        idx::NEW_OBJECT_ARRAY => {
            let len = x1 as usize;
            Ret::Obj(m.jni.handle_of(ObjKind::ObjectArray(vec![x3; len])))
        }
        idx::GET_OBJECT_ARRAY_ELEMENT => match m.jni.obj_of_handle(x1).map(|o| &o.kind) {
            Some(ObjKind::ObjectArray(v)) => Ret::Obj(v.get(x2 as usize).copied().unwrap_or(0)),
            _ => Ret::Obj(0),
        },
        idx::SET_OBJECT_ARRAY_ELEMENT => {
            let idxv = x2 as usize;
            if let Some(id) = m.jni.obj_id_of_handle(x1) {
                if let Some(o) = m.jni.objects.get_mut(&id) {
                    if let ObjKind::ObjectArray(v) = &mut o.kind {
                        if idxv < v.len() {
                            v[idxv] = x3;
                        }
                    }
                }
            }
            Ret::Void
        }
        idx::NEW_BYTE_ARRAY => Ret::Obj(m.jni.handle_of(ObjKind::ByteArray(vec![0; x1 as usize]))),
        175 | 177 | 178 | 179 | 180 | 181 | 182 => {
            // New{Boolean,Char,Short,Int,Long,Float,Double}Array
            let elem = match i {
                175 => 1u8,
                177 => 2,
                178 => 2,
                179 => 4,
                180 => 8,
                181 => 4,
                _ => 8,
            };
            let n = x1 as usize;
            Ret::Obj(m.jni.handle_of(ObjKind::PrimArray {
                elem,
                data: vec![0; n * elem as usize],
            }))
        }
        183..=190 => {
            // Get*ArrayElements
            let data = array_data(&m, x1);
            let addr = m.alloc_arena(data.len() as u64);
            let _ = uc.mem_write(addr, &data);
            if let Some(id) = m.jni.obj_id_of_handle(x1) {
                m.jni.pinned.insert(addr, id);
            }
            if x2 != 0 {
                let _ = uc.mem_write(x2, &[0u8; 1]);
            }
            Ret::Obj(addr)
        }
        191..=199 => {
            // Release*ArrayElements(array, elems, mode)
            release_pinned(uc, &mut m, x1, x2, x3 as i32);
            Ret::Void
        }
        200..=207 => {
            // Get*ArrayRegion(array, start, len, buf)
            let elem = elem_size_for_region(i - 200);
            let data = array_data(&m, x1);
            let start = (x2 as usize) * elem;
            let len = (x3 as usize) * elem;
            let slice = data
                .get(start..(start + len).min(data.len()))
                .unwrap_or(&[]);
            let _ = uc.mem_write(gx(uc, X4), slice);
            Ret::Void
        }
        208..=214 => {
            // Set*ArrayRegion(array, start, len, buf)
            let elem = elem_size_for_region(i - 208);
            let start = (x2 as usize) * elem;
            let len = (x3 as usize) * elem;
            let bytes = uc.mem_read_as_vec(gx(uc, X4), len).unwrap_or_default();
            if let Some(id) = m.jni.obj_id_of_handle(x1) {
                set_array_data(&mut m.jni, id, start, &bytes);
            }
            Ret::Void
        }
        idx::REGISTER_NATIVES => {
            let class = m.jni.class_name_of_handle(x1).unwrap_or_default();
            let count = x3 as usize;
            for k in 0..count {
                let entry = x2 + (k as u64) * 24;
                let raw = uc.mem_read_as_vec(entry, 24).unwrap_or_default();
                let rd = |o: usize| u64::from_le_bytes(raw[o..o + 8].try_into().unwrap());
                let (np, sp, fp) = (rd(0), rd(8), rd(16));
                let name = read_cstr(uc, np, 256).unwrap_or_default();
                let sig = read_cstr(uc, sp, 512).unwrap_or_default();
                eprintln!(
                    "[jni] RegisterNatives {}.{} {} -> {:#x}",
                    class, name, sig, fp
                );
                m.jni.natives.insert((class.clone(), name, sig), fp);
            }
            Ret::Int(0)
        }
        219 => {
            // GetJavaVM
            let vm = m.jni.vm_addr;
            let _ = uc.mem_write(x1, &vm.to_le_bytes());
            Ret::Int(0)
        }
        222 => {
            // GetPrimitiveArrayCritical
            let data = array_data(&m, x1);
            let addr = m.alloc_arena(data.len() as u64);
            let _ = uc.mem_write(addr, &data);
            if let Some(id) = m.jni.obj_id_of_handle(x1) {
                m.jni.pinned.insert(addr, id);
            }
            Ret::Obj(addr)
        }
        223 => {
            release_pinned(uc, &mut m, x1, x2, x3 as i32);
            Ret::Void
        }
        224 => {
            // GetStringCritical
            let s = string_content(&m, x1);
            let addr = m.alloc_arena((s.len() as u64 + 1) * 2);
            let mut b = Vec::new();
            for u in s.encode_utf16() {
                b.extend_from_slice(&u.to_le_bytes());
            }
            b.extend_from_slice(&[0, 0]);
            let _ = uc.mem_write(addr, &b);
            Ret::Obj(addr)
        }
        225 => Ret::Void,         // ReleaseStringCritical
        217 | 218 => Ret::Int(0), // MonitorEnter/Exit
        229 => {
            // NewDirectByteBuffer(address, capacity)
            let tag = format!("bb:{:#x}:{}", x1, x2);
            Ret::Obj(m.jni.handle_of(ObjKind::Named {
                class: "java/nio/ByteBuffer".to_string(),
                tag,
            }))
        }
        230 => {
            // GetDirectBufferAddress
            let tag = match m.jni.obj_of_handle(x1).map(|o| &o.kind) {
                Some(ObjKind::Named { tag, .. }) => tag.clone(),
                _ => String::new(),
            };
            let addr = tag
                .strip_prefix("bb:")
                .and_then(|t| t.split(':').next())
                .and_then(|h| u64::from_str_radix(h.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0);
            Ret::Long(addr as i64)
        }
        231 => {
            let tag = match m.jni.obj_of_handle(x1).map(|o| &o.kind) {
                Some(ObjKind::Named { tag, .. }) => tag.clone(),
                _ => String::new(),
            };
            let cap = tag
                .split(':')
                .nth(2)
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            Ret::Long(cap)
        }
        idx::GET_OBJECT_REF_TYPE => Ret::Int(1),
        _ => {
            eprintln!(
                "[jni] UNHANDLED fn {} (x1={:#x} x2={:#x} x3={:#x})",
                i, x1, x2, x3
            );
            Ret::Int(0)
        }
    }
}

fn elem_size_for_region(off: usize) -> usize {
    // Order: Boolean, Byte, Char, Short, Int, Long, Float, Double
    [1, 1, 2, 2, 4, 8, 4, 8][off.min(7)]
}

fn array_data(m: &Machine, handle: u64) -> Vec<u8> {
    match m.jni.obj_of_handle(handle).map(|o| &o.kind) {
        Some(ObjKind::ByteArray(v)) => v.clone(),
        Some(ObjKind::PrimArray { data, .. }) => data.clone(),
        _ => Vec::new(),
    }
}

fn set_array_data(jni: &mut JniState, id: u32, start: usize, bytes: &[u8]) {
    if let Some(o) = jni.objects.get_mut(&id) {
        let data = match &mut o.kind {
            ObjKind::ByteArray(v) => v,
            ObjKind::PrimArray { data, .. } => data,
            _ => return,
        };
        if start + bytes.len() > data.len() {
            data.resize(start + bytes.len(), 0);
        }
        data[start..start + bytes.len()].copy_from_slice(bytes);
    }
}

fn release_pinned(uc: &mut Uc<'_>, m: &mut Machine, _array: u64, elems: u64, mode: i32) {
    if mode != 2 {
        if let Some(&id) = m.jni.pinned.get(&elems) {
            // Determine the current array length to copy back.
            let len = match m.jni.objects.get(&id).map(|o| &o.kind) {
                Some(ObjKind::ByteArray(v)) => v.len(),
                Some(ObjKind::PrimArray { data, .. }) => data.len(),
                _ => 0,
            };
            if len > 0 {
                let bytes = uc.mem_read_as_vec(elems, len).unwrap_or_default();
                set_array_data(&mut m.jni, id, 0, &bytes);
            }
            if mode == 0 {
                m.jni.pinned.remove(&elems);
            }
        }
    } else {
        m.jni.pinned.remove(&elems);
    }
}

pub fn string_content(m: &Machine, handle: u64) -> String {
    match m.jni.obj_of_handle(handle).map(|o| &o.kind) {
        Some(ObjKind::Str(s)) => s.clone(),
        Some(ObjKind::Named { tag, .. }) => tag.clone(),
        Some(ObjKind::SecException { msg, .. }) => msg.clone(),
        _ => String::new(),
    }
}

// ---------- Java object construction + method/field stubs ----------
// Faithful port of SgHarness.java (round19) dispatch tables, plus the
// unidbg built-ins the happy path relies on (String/Map plumbing).

const PKG: &str = "com.taobao.idlefish";
const APK_PATH: &str = "/data/app/com.taobao.idlefish-1/base.apk";

fn host_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn named(m: &mut Machine, class: &str, tag: &str) -> Ret {
    Ret::Obj(m.jni.handle_of(ObjKind::Named {
        class: class.to_string(),
        tag: tag.to_string(),
    }))
}

fn file_obj(m: &mut Machine, path: &str) -> Ret {
    named(m, "java/io/File", path)
}

fn ctx_obj(m: &mut Machine) -> Ret {
    named(m, "android/content/Context", "CTX")
}

pub fn construct_object(m: &mut Machine, class: &str, sig: &str, args: &[Arg]) -> u64 {
    let jni = &mut m.jni;
    match (class, sig) {
        ("java/util/HashMap", _) => jni.handle_of(ObjKind::HashMap(Default::default())),
        ("java/lang/Integer", _) => {
            jni.handle_of(ObjKind::Int(args.first().map(|a| a.i()).unwrap_or(0)))
        }
        ("java/lang/Boolean", _) => jni.handle_of(ObjKind::Bool(
            args.first().map(|a| a.g() != 0).unwrap_or(false),
        )),
        ("java/lang/Long", _) => jni.handle_of(ObjKind::Long(
            args.first().map(|a| a.g() as i64).unwrap_or(0),
        )),
        ("java/lang/Float", _) => jni.handle_of(ObjKind::Float(
            args.first()
                .map(|a| match a {
                    Arg::F(f) => *f,
                    Arg::G(v) => f32::from_bits(*v as u32),
                    Arg::D(d) => *d as f32,
                })
                .unwrap_or(0.0),
        )),
        ("java/lang/Double", _) => jni.handle_of(ObjKind::Double(
            args.first()
                .map(|a| match a {
                    Arg::D(d) => *d,
                    Arg::G(v) => f64::from_bits(*v),
                    Arg::F(f) => *f as f64,
                })
                .unwrap_or(0.0),
        )),
        ("com/alibaba/wireless/security/open/SecException", _) => {
            let msg = args.first().and_then(|a| a.s(jni)).unwrap_or_default();
            let code = args.get(1).map(|a| a.i()).unwrap_or(0);
            eprintln!(
                "[jni] SecException constructed by native: msg={} code={}",
                msg, code
            );
            jni.handle_of(ObjKind::SecException { msg, code })
        }
        _ => {
            if m.log_jni {
                eprintln!("[jni] newObject {} {} (generic)", class, sig);
            }
            jni.handle_of(ObjKind::Named {
                class: class.to_string(),
                tag: String::new(),
            })
        }
    }
}

/// The JNI "signature key" the Java harness dispatches on:
/// declaringClass->name + sig.
fn key(mid: &MethodId) -> String {
    format!("{}->{}{}", mid.class, mid.name, mid.sig)
}

pub fn call_java(
    uc: &mut Uc<'_>,
    m: &mut Machine,
    mid: &MethodId,
    obj: u64,
    args: &[Arg],
    is_static: bool,
) -> Ret {
    let _ = uc;
    let k = key(mid);
    if is_static {
        return call_static(m, mid, &k, obj, args);
    }
    call_instance(m, mid, &k, obj, args)
}

fn call_static(m: &mut Machine, mid: &MethodId, k: &str, _clazz: u64, args: &[Arg]) -> Ret {
    let jni = &mut m.jni;
    match k {
        "com/alibaba/wireless/security/mainplugin/SecurityGuardMainPlugin->getMainPluginClassLoader()Ljava/lang/ClassLoader;" =>
        {
            return named(m, "java/lang/ClassLoader", "CL");
        }
        "com/taobao/wireless/security/adapter/common/SPUtility2->readFromSPUnified(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"
        | "com/taobao/wireless/security/adapter/common/SPUtility2->read(Ljava/lang/String;)Ljava/lang/String;"
        | "com/taobao/wireless/security/adapter/common/SPUtility2->readSS(Landroid/content/Context;Ljava/lang/String;)Ljava/lang/String;"
        | "com/taobao/wireless/security/adapter/datacollection/DeviceInfoCapturer->doCommandForString(I)Ljava/lang/String;" =>
        {
            return Ret::Obj(jni.string_obj(""));
        }
        "com/taobao/wireless/security/adapter/common/HttpUtil->sendSyncHttpGetRequestBridge(Ljava/lang/String;)Ljava/lang/String;"
        | "com/taobao/wireless/security/adapter/common/HttpUtil->sendSyncHttpPostRequestBridge(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;II)Ljava/lang/String;"
        | "com/taobao/wireless/security/adapter/common/HttpUtil->downloadFileBridge(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;" =>
        {
            eprintln!("[jni] HttpUtil bridge called (returning empty; no network in harness)");
            return Ret::Obj(jni.string_obj(""));
        }
        "com/taobao/wireless/security/adapter/datareport/DataReportJniBridge->sendReportBridge(Ljava/lang/String;Ljava/lang/String;[B)Ljava/lang/String;"
        | "com/taobao/wireless/security/adapter/datareport/DataReportJniBridge->sendReportBridge(Ljava/lang/String;Ljava/lang/String;Ljava/util/Map;[B)Ljava/lang/String;" =>
        {
            return Ret::Obj(jni.string_obj(""));
        }
        "com/alibaba/wireless/security/open/edgecomputing/ProxyUtil->getProxyInstance(Ljava/lang/String;Ljava/lang/Object;I)Ljava/lang/Object;" =>
        {
            return Ret::Obj(0);
        }
        "com/alibaba/wireless/security/open/edgecomputing/ProxyUtil->getHandler()Landroid/os/Handler;" =>
        {
            return named(m, "android/os/Handler", "H");
        }
        _ => {}
    }
    // Return-type families with prefix rules, mirroring the Java
    // harness's callStatic{Boolean,Int,Long}Method fallbacks.
    let (_, ret) = parse_params(&mid.sig);
    match ret {
        P::Bool => {
            match k {
                "com/alibaba/wireless/security/framework/ApmMonitorAdapter->isEnableFullTrackRecord()Z"
                | "com/alibaba/wireless/security/framework/ApmMonitorAdapter->isForeground()Z" => Ret::Bool(false),
                _ if k.starts_with("com/taobao/wireless/security/adapter/common/SPUtility2->removeFromSPUnified")
                    || k.starts_with("com/taobao/wireless/security/adapter/common/SPUtility2->writeSS") => Ret::Bool(true),
                _ => {
                    log_unhandled(m, "callStaticBooleanMethod", k);
                    Ret::Bool(false)
                }
            }
        }
        P::Int => {
            if k.starts_with("com/taobao/wireless/security/adapter/datareport/DataReportJniBridge->")
                || k.starts_with("com/taobao/wireless/security/adapter/common/SPUtility2->saveToFileUnifiedForNative") {
                Ret::Int(0)
            } else {
                log_unhandled(m, "callStaticIntMethod", k);
                Ret::Int(0)
            }
        }
        P::Long => {
            match k {
                "android/os/SystemClock->elapsedRealtime()J" => Ret::Long(123456789),
                "android/os/SystemClock->uptimeMillis()J" => Ret::Long(host_millis() % 100_000_000),
                _ => {
                    log_unhandled(m, "callStaticLongMethod", k);
                    Ret::Long(0)
                }
            }
        }
        P::Void => {
            match k {
                "com/taobao/wireless/security/adapter/common/SPUtility2->write(Ljava/lang/String;Ljava/lang/String;)V"
                | "com/alibaba/wireless/security/open/edgecomputing/ECMiscInfo->registerAppLifeCyCleCallBack()V" => Ret::Void,
                _ => {
                    log_unhandled(m, "callStaticVoidMethod", k);
                    Ret::Void
                }
            }
        }
        P::Obj => {
            // Class.forName / ClassLoader.getSystemClassLoader and
            // similar unidbg built-ins the native code may use.
            if mid.class == "java/lang/Class" && mid.name == "forName" {
                let name = args.first().and_then(|a| a.s(jni)).unwrap_or_default().replace('.', "/");
                return Ret::Obj(jni.class_handle(&name));
            }
            log_unhandled(m, "callStaticObjectMethod", k);
            Ret::Obj(0)
        }
        _ => {
            log_unhandled(m, "callStaticMethod", k);
            Ret::Int(0)
        }
    }
}

fn log_unhandled(m: &Machine, what: &str, k: &str) {
    if m.log_jni {
        eprintln!("[jni] UNHANDLED {}: {}", what, k);
    }
}

fn call_instance(m: &mut Machine, mid: &MethodId, k: &str, obj: u64, args: &[Arg]) -> Ret {
    // Map plumbing (values are Strings on this path).
    if matches!(
        k,
        "java/util/HashMap->get(Ljava/lang/Object;)Ljava/lang/Object;"
            | "java/util/Map->get(Ljava/lang/Object;)Ljava/lang/Object;"
    ) {
        let key = args.first().and_then(|a| a.s(&m.jni)).unwrap_or_default();
        let v = match m.jni.obj_of_handle(obj).map(|o| &o.kind) {
            Some(ObjKind::HashMap(map)) => map.get(&key).cloned(),
            _ => None,
        };
        if m.log_jni {
            eprintln!("[jni] Map.get({}) -> {:?}", key, v);
        }
        return match v {
            Some(s) => Ret::Obj(m.jni.string_obj(&s)),
            None => Ret::Obj(0),
        };
    }
    if matches!(
        k,
        "java/util/HashMap->put(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            | "java/util/Map->put(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
    ) {
        let key = args.first().and_then(|a| a.s(&m.jni)).unwrap_or_default();
        let val = args.get(1).and_then(|a| a.s(&m.jni)).unwrap_or_default();
        let mut prev = None;
        if let Some(id) = m.jni.obj_id_of_handle(obj) {
            if let Some(o) = m.jni.objects.get_mut(&id) {
                if let ObjKind::HashMap(map) = &mut o.kind {
                    prev = map.insert(key, val);
                }
            }
        }
        return match prev {
            Some(s) => Ret::Obj(m.jni.string_obj(&s)),
            None => Ret::Obj(0),
        };
    }
    if matches!(
        k,
        "java/util/HashMap->keySet()Ljava/util/Set;" | "java/util/Map->keySet()Ljava/util/Set;"
    ) {
        let keys = match m.jni.obj_of_handle(obj).map(|o| &o.kind) {
            Some(ObjKind::HashMap(map)) => map.keys().cloned().collect::<Vec<String>>(),
            _ => Vec::new(),
        };
        return Ret::Obj(m.jni.handle_of(ObjKind::StrSet(keys)));
    }
    if matches!(
        k,
        "java/util/Set->toArray()[Ljava/lang/Object;"
            | "java/util/HashSet->toArray()[Ljava/lang/Object;"
    ) {
        let elems = match m.jni.obj_of_handle(obj).map(|o| &o.kind) {
            Some(ObjKind::StrSet(keys)) => keys.clone(),
            _ => Vec::new(),
        };
        let mut handles = Vec::new();
        for e in &elems {
            handles.push(m.jni.string_obj(e));
        }
        return Ret::Obj(m.jni.handle_of(ObjKind::ObjectArray(handles)));
    }
    if k == "java/lang/String->getBytes(Ljava/lang/String;)[B" {
        let s = string_content(m, obj);
        return Ret::Obj(m.jni.handle_of(ObjKind::ByteArray(s.into_bytes())));
    }
    if k == "java/lang/ClassLoader->loadClass(Ljava/lang/String;)Ljava/lang/Class;" {
        let name = args.first().and_then(|a| a.s(&m.jni)).unwrap_or_default();
        if m.log_jni {
            eprintln!("[jni] ClassLoader.loadClass({})", name);
        }
        return Ret::Obj(m.jni.class_handle(&name.replace('.', "/")));
    }
    if mid.class == "android/content/Context" || mid.class == "android/content/ContextWrapper" {
        return context_call(m, k, args);
    }
    if mid.class == "java/io/File" {
        return file_call(m, mid, k, obj, args);
    }
    // Boxed unboxing + container sizes + misc instance rules.
    let (_, ret) = parse_params(&mid.sig);
    match ret {
        P::Int => {
            if k == "java/lang/Integer->intValue()I" {
                if let Some(ObjKind::Int(v)) = m.jni.obj_of_handle(obj).map(|o| &o.kind) {
                    return Ret::Int(*v);
                }
            }
            if (k == "java/util/HashMap->size()I" || k == "java/util/Map->size()I") {
                if let Some(ObjKind::HashMap(map)) = m.jni.obj_of_handle(obj).map(|o| &o.kind) {
                    return Ret::Int(map.len() as i32);
                }
            }
            if mid.class == "android/content/SharedPreferences" && mid.name.starts_with("getInt") {
                return Ret::Int(0);
            }
            if k == "android/content/Context->checkSelfPermission(Ljava/lang/String;)I"
                || k == "android/content/Context->checkCallingOrSelfPermission(Ljava/lang/String;)I"
            {
                return Ret::Int(0);
            }
            // unidbg built-ins for String/Object used on many paths.
            if mid.class == "java/lang/String" {
                match mid.name.as_str() {
                    "length" => {
                        return Ret::Int(string_content(m, obj).encode_utf16().count() as i32);
                    }
                    "hashCode" => {
                        let s = string_content(m, obj);
                        let mut h: i32 = 0;
                        for u in s.encode_utf16() {
                            h = h.wrapping_mul(31).wrapping_add(u as i32);
                        }
                        return Ret::Int(h);
                    }
                    "indexOf" => return Ret::Int(-1),
                    _ => {}
                }
            }
            log_unhandled(m, "callIntMethod", k);
            Ret::Int(0)
        }
        P::Bool => {
            if k == "java/lang/Boolean->booleanValue()Z" {
                if let Some(ObjKind::Bool(v)) = m.jni.obj_of_handle(obj).map(|o| &o.kind) {
                    return Ret::Bool(*v);
                }
            }
            if mid.class == "java/lang/String" && mid.name == "equals" {
                let a = string_content(m, obj);
                let b = args
                    .first()
                    .map(|x| string_content(m, x.g()))
                    .unwrap_or_default();
                return Ret::Bool(a == b);
            }
            if mid.class == "java/lang/Object" && mid.name == "equals" {
                let a = m.jni.obj_id_of_handle(obj);
                let b = args.first().and_then(|x| m.jni.obj_id_of_handle(x.g()));
                return Ret::Bool(a == b && a.is_some());
            }
            if mid.class == "java/io/File"
                && matches!(
                    mid.name.as_str(),
                    "exists" | "mkdirs" | "mkdir" | "canRead" | "canWrite"
                )
            {
                return Ret::Bool(true);
            }
            log_unhandled(m, "callBooleanMethod", k);
            Ret::Bool(false)
        }
        P::Long => {
            if k == "java/lang/Long->longValue()J" {
                if let Some(ObjKind::Long(v)) = m.jni.obj_of_handle(obj).map(|o| &o.kind) {
                    return Ret::Long(*v);
                }
            }
            if k == "java/io/File->length()J" {
                return Ret::Long(0);
            }
            if k == "java/io/File->lastModified()J" {
                return Ret::Long(host_millis());
            }
            log_unhandled(m, "callLongMethod", k);
            Ret::Long(0)
        }
        P::Float => {
            if let Some(ObjKind::Float(v)) = m.jni.obj_of_handle(obj).map(|o| &o.kind) {
                if mid.name == "floatValue" {
                    return Ret::Float(*v);
                }
            }
            Ret::Float(0.0)
        }
        P::Double => {
            if let Some(ObjKind::Double(v)) = m.jni.obj_of_handle(obj).map(|o| &o.kind) {
                if mid.name == "doubleValue" {
                    return Ret::Double(*v);
                }
            }
            Ret::Double(0.0)
        }
        P::Void => {
            if m.log_jni {
                eprintln!("[jni] void method (no-op): {}", k);
            }
            Ret::Void
        }
        P::Obj => {
            // Common unidbg-handled object methods.
            if mid.class == "java/lang/Object" && mid.name == "toString" {
                let s = match m.jni.obj_of_handle(obj).map(|o| &o.kind) {
                    Some(ObjKind::Str(s)) => s.clone(),
                    Some(ObjKind::Named { class, tag }) => {
                        if tag.is_empty() {
                            class.clone()
                        } else {
                            tag.clone()
                        }
                    }
                    _ => obj_class_name(&m.jni, obj),
                };
                return Ret::Obj(m.jni.string_obj(&s));
            }
            if mid.class == "java/lang/String" && mid.name == "toString" {
                return Ret::Obj(obj);
            }
            if mid.class == "java/lang/Class" && mid.name == "getName" {
                let n = m.jni.class_name_of_handle(obj).unwrap_or_default();
                return Ret::Obj(m.jni.string_obj(&n));
            }
            if mid.name == "getClass" {
                let n = obj_class_name(&m.jni, obj);
                return Ret::Obj(m.jni.class_handle(&n));
            }
            log_unhandled(m, "callObjectMethod", k);
            Ret::Obj(0)
        }
        _ => Ret::Int(0),
    }
}

fn context_call(m: &mut Machine, k: &str, args: &[Arg]) -> Ret {
    let jni = &mut m.jni;
    match k {
        "android/content/Context->getPackageName()Ljava/lang/String;"
        | "android/content/ContextWrapper->getPackageName()Ljava/lang/String;" => {
            Ret::Obj(jni.string_obj(PKG))
        }
        "android/content/Context->getPackageCodePath()Ljava/lang/String;"
        | "android/content/ContextWrapper->getPackageCodePath()Ljava/lang/String;"
        | "android/content/Context->getPackageResourcePath()Ljava/lang/String;" => {
            Ret::Obj(jni.string_obj(APK_PATH))
        }
        "android/content/Context->getApplicationContext()Landroid/content/Context;"
        | "android/content/ContextWrapper->getApplicationContext()Landroid/content/Context;"
        | "android/content/ContextWrapper->getBaseContext()Landroid/content/Context;" => ctx_obj(m),
        "android/content/Context->getFilesDir()Ljava/io/File;"
        | "android/content/ContextWrapper->getFilesDir()Ljava/io/File;" => {
            file_obj(m, &format!("/data/data/{}/files", PKG))
        }
        "android/content/Context->getCacheDir()Ljava/io/File;" => {
            file_obj(m, &format!("/data/data/{}/cache", PKG))
        }
        "android/content/Context->getDir(Ljava/lang/String;I)Ljava/io/File;" => {
            let name = args.first().and_then(|a| a.s(jni)).unwrap_or_default();
            file_obj(m, &format!("/data/data/{}/app_{}", PKG, name))
        }
        "android/content/Context->getExternalFilesDir(Ljava/lang/String;)Ljava/io/File;" => {
            file_obj(m, &format!("/sdcard/Android/data/{}/files", PKG))
        }
        "android/content/Context->getPackageManager()Landroid/content/pm/PackageManager;"
        | "android/content/ContextWrapper->getPackageManager()Landroid/content/pm/PackageManager;" => {
            named(m, "android/content/pm/PackageManager", "PM")
        }
        "android/content/Context->getApplicationInfo()Landroid/content/pm/ApplicationInfo;" => {
            named(m, "android/content/pm/ApplicationInfo", "APPINFO")
        }
        "android/content/Context->getContentResolver()Landroid/content/ContentResolver;" => {
            named(m, "android/content/ContentResolver", "CR")
        }
        "android/content/Context->getSystemService(Ljava/lang/String;)Ljava/lang/Object;" => {
            let name = args.first().and_then(|a| a.s(jni)).unwrap_or_default();
            if m.log_jni {
                eprintln!("[jni] Context.getSystemService({}) -> null-object", name);
            }
            named(m, "java/lang/Object", "")
        }
        "android/content/Context->getSharedPreferences(Ljava/lang/String;I)Landroid/content/SharedPreferences;" => {
            Ret::Obj(jni.handle_of(ObjKind::HashMap(Default::default())))
        }
        "android/content/Context->getClassLoader()Ljava/lang/ClassLoader;" => {
            named(m, "java/lang/ClassLoader", "CL")
        }
        "android/content/Context->getResources()Landroid/content/res/Resources;" => {
            named(m, "android/content/res/Resources", "RES")
        }
        "android/content/Context->getAssets()Landroid/content/res/AssetManager;" => {
            named(m, "android/content/res/AssetManager", "AM")
        }
        _ => {
            log_unhandled(m, "Context call", k);
            // Fall back on return-type defaults like the Java harness's null.
            Ret::Obj(0)
        }
    }
}

fn file_call(m: &mut Machine, mid: &MethodId, k: &str, obj: u64, _args: &[Arg]) -> Ret {
    let path = string_content(m, obj);
    match k {
        "java/io/File->getAbsolutePath()Ljava/lang/String;"
        | "java/io/File->getPath()Ljava/lang/String;" => Ret::Obj(m.jni.string_obj(&path)),
        "java/io/File->getName()Ljava/lang/String;" => {
            let name = path.rsplit('/').next().unwrap_or("").to_string();
            Ret::Obj(m.jni.string_obj(&name))
        }
        "java/io/File->getParent()Ljava/lang/String;" => {
            let parent = match path.rfind('/') {
                Some(0) => "/".to_string(),
                Some(i) => path[..i].to_string(),
                None => "null".to_string(),
            };
            Ret::Obj(m.jni.string_obj(&parent))
        }
        _ => match mid.name.as_str() {
            "exists" | "mkdirs" | "mkdir" | "canRead" | "canWrite" => Ret::Bool(true),
            "length" => Ret::Long(0),
            "lastModified" => Ret::Long(host_millis()),
            "isDirectory" | "isFile" | "delete" | "createNewFile" => Ret::Bool(false),
            _ => {
                log_unhandled(m, "File call", k);
                Ret::Obj(0)
            }
        },
    }
}

pub fn get_java_field(
    uc: &mut Uc<'_>,
    m: &mut Machine,
    fid: &crate::jni::FieldId,
    obj: u64,
    is_static: bool,
) -> Ret {
    let _ = uc;
    let _ = obj;
    let k = format!("{}->{}:{}", fid.class, fid.name, fid.sig);
    if is_static {
        if fid.class == "android/os/Build" || fid.class == "android/os/Build$VERSION" {
            let v = match k.as_str() {
                "android/os/Build->BRAND:Ljava/lang/String;" => "Xiaomi",
                "android/os/Build->MODEL:Ljava/lang/String;" => "MI 9",
                "android/os/Build->MANUFACTURER:Ljava/lang/String;" => "Xiaomi",
                "android/os/Build->PRODUCT:Ljava/lang/String;" => "cepheus",
                "android/os/Build->DEVICE:Ljava/lang/String;" => "cepheus",
                "android/os/Build->BOARD:Ljava/lang/String;" => "msmnile",
                "android/os/Build->HARDWARE:Ljava/lang/String;" => "qcom",
                "android/os/Build->FINGERPRINT:Ljava/lang/String;" => {
                    "Xiaomi/cepheus/cepheus:10/QKQ1.190825.002/V12.0.4.0.QFACNXM:user/release-keys"
                }
                "android/os/Build->ID:Ljava/lang/String;" => "QKQ1.190825.002",
                "android/os/Build->DISPLAY:Ljava/lang/String;" => "QKQ1.190825.002",
                "android/os/Build->HOST:Ljava/lang/String;" => "builder",
                "android/os/Build->USER:Ljava/lang/String;" => "builder",
                "android/os/Build->TAGS:Ljava/lang/String;" => "release-keys",
                "android/os/Build->TYPE:Ljava/lang/String;" => "user",
                "android/os/Build->SERIAL:Ljava/lang/String;" => "unknown",
                "android/os/Build$VERSION->RELEASE:Ljava/lang/String;" => "10",
                "android/os/Build$VERSION->CODENAME:Ljava/lang/String;" => "REL",
                "android/os/Build$VERSION->INCREMENTAL:Ljava/lang/String;" => "V12.0.4.0.QFACNXM",
                "android/os/Build$VERSION->SECURITY_PATCH:Ljava/lang/String;" => "2020-09-01",
                _ => {
                    log_unhandled(m, "Build field", &k);
                    ""
                }
            };
            return Ret::Obj(m.jni.string_obj(v));
        }
        if k == "android/os/Build$VERSION->SDK_INT:I" {
            return Ret::Int(29);
        }
        log_unhandled(m, "getStaticField", &k);
        return match fid.sig.as_str() {
            "I" => Ret::Int(0),
            "J" => Ret::Long(0),
            "Z" => Ret::Bool(false),
            _ => Ret::Obj(0),
        };
    }
    match k.as_str() {
        "android/content/pm/ApplicationInfo->sourceDir:Ljava/lang/String;"
        | "android/content/pm/ApplicationInfo->publicSourceDir:Ljava/lang/String;" => {
            return Ret::Obj(m.jni.string_obj(APK_PATH));
        }
        "android/content/pm/ApplicationInfo->nativeLibraryDir:Ljava/lang/String;" => {
            return Ret::Obj(m.jni.string_obj(&format!("/data/app/{}-1/lib/arm64", PKG)));
        }
        "android/content/pm/ApplicationInfo->dataDir:Ljava/lang/String;" => {
            return Ret::Obj(m.jni.string_obj(&format!("/data/data/{}", PKG)));
        }
        "android/content/pm/PackageInfo->versionName:Ljava/lang/String;" => {
            return Ret::Obj(m.jni.string_obj("7.28.70"));
        }
        "android/content/pm/ApplicationInfo->uid:I" => return Ret::Int(10234),
        "android/content/pm/PackageInfo->versionCode:I" => return Ret::Int(469),
        _ => {}
    }
    log_unhandled(m, "getObjectField", &k);
    match fid.sig.as_str() {
        "I" => Ret::Int(0),
        "J" => Ret::Long(0),
        "Z" => Ret::Bool(false),
        _ => Ret::Obj(0),
    }
}

pub fn set_java_field(
    m: &mut Machine,
    fid: &crate::jni::FieldId,
    _obj: u64,
    _val: &Arg,
    _is_static: bool,
) {
    if m.log_jni {
        eprintln!(
            "[jni] setfield {}.{} {} (ignored)",
            fid.class, fid.name, fid.sig
        );
    }
}
