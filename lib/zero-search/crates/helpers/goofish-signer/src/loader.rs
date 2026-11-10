//! Minimal ELF64/AArch64 dynamic loader data structures + parser.
//! Parses purely from program headers and the dynamic segment: the SG
//! library's section headers are deliberately mangled, so any
//! section-driven parser (readelf, goblin's Elf::parse) fails on it.

pub const R_AARCH64_ABS64: u32 = 257;
pub const R_AARCH64_GLOB_DAT: u32 = 1025;
pub const R_AARCH64_JUMP_SLOT: u32 = 1026;
pub const R_AARCH64_RELATIVE: u32 = 1027;
pub const R_AARCH64_TPREL64: u32 = 1030;
pub const R_AARCH64_IRELATIVE: u32 = 1032;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_TLS: u32 = 7;

const DT_NEEDED: i64 = 1;
const DT_PLTRELSZ: i64 = 2;
const DT_HASH: i64 = 4;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_STRSZ: i64 = 10;
const DT_SYMENT: i64 = 11;
const DT_INIT: i64 = 12;
const DT_FINI: i64 = 13;
const DT_SONAME: i64 = 14;
const DT_REL: i64 = 17;
const DT_RELSZ: i64 = 18;
const DT_JMPREL: i64 = 23;
const DT_INIT_ARRAY: i64 = 25;
const DT_FINI_ARRAY: i64 = 26;
const DT_INIT_ARRAYSZ: i64 = 27;
const DT_FINI_ARRAYSZ: i64 = 28;
const DT_PREINIT_ARRAY: i64 = 32;
const DT_PREINIT_ARRAYSZ: i64 = 33;
const DT_GNU_HASH: i64 = 0x6ffffef5;

#[derive(Clone, Debug)]
pub struct DynSym {
    pub name: String,
    pub value: u64,
    pub size: u64,
    pub shndx: u16,
    pub bind: u8,
    pub typ: u8,
}

#[derive(Clone, Debug)]
pub struct Reloc {
    pub offset: u64,
    pub rtype: u32,
    pub sym: usize,
    pub addend: i64,
}

#[derive(Clone, Debug)]
pub struct LoadSeg {
    pub vaddr: u64,
    pub offset: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub flags: u32,
}

pub struct ModuleImage {
    pub name: String,
    pub data: Vec<u8>,
    pub loads: Vec<LoadSeg>,
    pub span: u64,
    pub needed: Vec<String>,
    pub soname: String,
    pub init: Option<u64>,
    pub fini: Option<u64>,
    pub preinit_array: Vec<u64>,
    pub init_array: Vec<u64>,
    /// (vaddr, size) of INIT_ARRAY / PREINIT_ARRAY: file contents are
    /// zero placeholders filled by RELATIVE relocations, so callers
    /// must read the entries back from relocated guest memory.
    pub init_array_addr: Option<(u64, u64)>,
    pub preinit_array_addr: Option<(u64, u64)>,
    pub fini_array: Vec<u64>,
    pub symbols: Vec<DynSym>,
    pub relocs: Vec<Reloc>,
    pub tls: Option<(u64, u64, u64, u64)>, // vaddr, filesz, memsz, align
}

fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) & !(a - 1)
}

fn rd_u16(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(d[o..o + 2].try_into().unwrap())
}
fn rd_u32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}
fn rd_u64(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn rd_i64(d: &[u8], o: usize) -> i64 {
    rd_u64(d, o) as i64
}

fn parse_rela(relocs: &mut Vec<Reloc>, loads: &[LoadSeg], vaddr: u64, size: u64, data: &[u8]) {
    let v2o = |vaddr: u64| -> Option<usize> {
        for seg in loads {
            if vaddr >= seg.vaddr && vaddr < seg.vaddr + seg.filesz {
                return Some((seg.offset + (vaddr - seg.vaddr)) as usize);
            }
        }
        None
    };
    if let Some(mut ro) = v2o(vaddr) {
        let count = (size / 24) as usize;
        for _ in 0..count {
            let r_offset = rd_u64(data, ro);
            let r_info = rd_u64(data, ro + 8);
            let r_addend = rd_i64(data, ro + 16);
            relocs.push(Reloc {
                offset: r_offset,
                rtype: (r_info & 0xffff_ffff) as u32,
                sym: (r_info >> 32) as usize,
                addend: r_addend,
            });
            ro += 24;
        }
    }
}

impl ModuleImage {
    pub fn is_exec(flags: u32) -> bool {
        flags & 1 != 0
    }

    pub fn v2o(&self, vaddr: u64) -> Option<usize> {
        for seg in &self.loads {
            if vaddr >= seg.vaddr && vaddr < seg.vaddr + seg.filesz {
                return Some((seg.offset + (vaddr - seg.vaddr)) as usize);
            }
        }
        None
    }

    pub fn parse(name: &str, data: Vec<u8>) -> Result<ModuleImage, String> {
        let d = &data;
        if d.len() < 64 || &d[0..4] != b"\x7fELF" || d[4] != 2 || d[5] != 1 {
            return Err(format!("{}: not an ELF64 LE file", name));
        }
        let e_phoff = rd_u64(d, 0x20) as usize;
        let e_phentsize = rd_u16(d, 0x36) as usize;
        let e_phnum = rd_u16(d, 0x38) as usize;
        let mut loads = Vec::new();
        let mut span = 0u64;
        let mut tls = None;
        let mut dyn_seg: Option<(u64, u64)> = None; // file offset, filesz
        for i in 0..e_phnum {
            let o = e_phoff + i * e_phentsize;
            let p_type = rd_u32(d, o);
            let p_flags = rd_u32(d, o + 4);
            let p_offset = rd_u64(d, o + 8);
            let p_vaddr = rd_u64(d, o + 16);
            let p_filesz = rd_u64(d, o + 32);
            let p_memsz = rd_u64(d, o + 40);
            let p_align = rd_u64(d, o + 48);
            match p_type {
                PT_LOAD => {
                    loads.push(LoadSeg {
                        vaddr: p_vaddr,
                        offset: p_offset,
                        filesz: p_filesz,
                        memsz: p_memsz,
                        flags: p_flags,
                    });
                    span = span.max(p_vaddr + p_memsz);
                }
                PT_DYNAMIC => dyn_seg = Some((p_offset, p_filesz)),
                PT_TLS => tls = Some((p_vaddr, p_filesz, p_memsz, p_align)),
                _ => {}
            }
        }
        span = align_up(span, 0x1000);
        let (dyn_off, dyn_sz) = dyn_seg.ok_or_else(|| format!("{}: no PT_DYNAMIC", name))?;
        let mut tags: std::collections::HashMap<i64, Vec<u64>> = std::collections::HashMap::new();
        let mut o = dyn_off as usize;
        let dyn_end = o + dyn_sz as usize;
        while o + 16 <= dyn_end {
            let tag = rd_i64(d, o);
            let val = rd_u64(d, o + 8);
            o += 16;
            if tag == 0 {
                break;
            }
            tags.entry(tag).or_default().push(val);
        }
        let get = |t: i64| tags.get(&t).and_then(|v| v.first().copied());
        // We need v2o before building the full struct; do a local helper.
        let v2o = |vaddr: u64, loads: &[LoadSeg]| -> Option<usize> {
            for seg in loads {
                if vaddr >= seg.vaddr && vaddr < seg.vaddr + seg.filesz {
                    return Some((seg.offset + (vaddr - seg.vaddr)) as usize);
                }
            }
            None
        };
        // String table
        let strtab_off = get(DT_STRTAB)
            .and_then(|v| v2o(v, &loads))
            .ok_or("no DT_STRTAB")?;
        let strsz = get(DT_STRSZ).unwrap_or(0) as usize;
        let strtab = &d[strtab_off..(strtab_off + strsz).min(d.len())];
        let get_str = |off: usize| -> String {
            if off >= strtab.len() {
                return String::new();
            }
            let end = strtab[off..]
                .iter()
                .position(|&c| c == 0)
                .map(|p| off + p)
                .unwrap_or(strtab.len());
            String::from_utf8_lossy(&strtab[off..end]).into_owned()
        };
        let mut needed = Vec::new();
        if let Some(vals) = tags.get(&DT_NEEDED) {
            for &v in vals {
                needed.push(get_str(v as usize));
            }
        }
        let soname = get(DT_SONAME)
            .map(|v| get_str(v as usize))
            .unwrap_or_else(|| name.to_string());
        // Symbol count: DT_HASH nchain is authoritative when present.
        let symcount = if let Some(h) = get(DT_HASH).and_then(|v| v2o(v, &loads)) {
            rd_u32(d, h + 4) as usize
        } else if let Some(g) = get(DT_GNU_HASH).and_then(|v| v2o(v, &loads)) {
            // Walk the GNU hash chain of the last bucket to find the max symbol.
            let nbuckets = rd_u32(d, g) as usize;
            let symoffset = rd_u32(d, g + 4) as usize;
            let bloom_size = rd_u32(d, g + 8) as usize;
            let buckets_off = g + 16 + bloom_size * 8;
            let mut max_sym = symoffset;
            let chain_off = buckets_off + nbuckets * 4;
            for i in 0..nbuckets {
                let b = rd_u32(d, buckets_off + i * 4) as usize;
                if b >= symoffset && b > 0 {
                    // Walk the whole chain: bucket chains are NOT
                    // ordered by bucket index, so no early skip.
                    let mut idx = b;
                    loop {
                        let cv = rd_u32(d, chain_off + (idx - symoffset) * 4);
                        idx += 1;
                        if cv & 1 == 1 {
                            break;
                        }
                    }
                    if idx > max_sym {
                        max_sym = idx;
                    }
                }
            }
            max_sym
        } else {
            0
        };
        let syment = get(DT_SYMENT).unwrap_or(24) as usize;
        let symtab_off = get(DT_SYMTAB)
            .and_then(|v| v2o(v, &loads))
            .ok_or("no DT_SYMTAB")?;
        let mut symbols = Vec::with_capacity(symcount);
        for i in 0..symcount {
            let so = symtab_off + i * syment;
            let st_name = rd_u32(d, so) as usize;
            let st_info = d[so + 4];
            let st_shndx = rd_u16(d, so + 6);
            let st_value = rd_u64(d, so + 8);
            let st_size = rd_u64(d, so + 16);
            symbols.push(DynSym {
                name: get_str(st_name),
                value: st_value,
                size: st_size,
                shndx: st_shndx,
                bind: st_info >> 4,
                typ: st_info & 0xf,
            });
        }
        // Relocations (RELA only on AArch64 Android; REL handled defensively).
        let mut relocs = Vec::new();
        if let (Some(a), Some(sz)) = (get(DT_RELA), get(DT_RELASZ)) {
            parse_rela(&mut relocs, &loads, a, sz, d);
        }
        if let (Some(a), Some(sz)) = (get(DT_JMPREL), get(DT_PLTRELSZ)) {
            // JMPREL may or may not overlap the DT_RELA range; dedupe by offset+type+sym.
            parse_rela(&mut relocs, &loads, a, sz, d);
            let mut seen = std::collections::HashSet::new();
            let mut dedup: Vec<Reloc> = Vec::with_capacity(relocs.len());
            for r in relocs.drain(..) {
                if seen.insert((r.offset, r.rtype, r.sym)) {
                    dedup.push(r);
                }
            }
            relocs = dedup;
        }
        if get(DT_REL).is_some() && get(DT_RELSZ).map(|s| s > 0).unwrap_or(false) {
            return Err(format!("{}: DT_REL present (unexpected on AArch64)", name));
        }
        let read_ptr_arr = |addr: Option<u64>, sz: u64| -> Vec<u64> {
            let mut out = Vec::new();
            if let (Some(a), true) = (addr, sz > 0) {
                if let Some(off) = v2o(a, &loads) {
                    for i in 0..(sz / 8) as usize {
                        out.push(rd_u64(d, off + i * 8));
                    }
                }
            }
            out
        };
        let init_array = read_ptr_arr(get(DT_INIT_ARRAY), get(DT_INIT_ARRAYSZ).unwrap_or(0));
        let fini_array = read_ptr_arr(get(DT_FINI_ARRAY), get(DT_FINI_ARRAYSZ).unwrap_or(0));
        let preinit_array =
            read_ptr_arr(get(DT_PREINIT_ARRAY), get(DT_PREINIT_ARRAYSZ).unwrap_or(0));
        Ok(ModuleImage {
            name: name.to_string(),
            data,
            loads,
            span,
            needed,
            soname,
            init: get(DT_INIT),
            fini: get(DT_FINI),
            preinit_array,
            init_array,
            init_array_addr: get(DT_INIT_ARRAY).map(|a| (a, get(DT_INIT_ARRAYSZ).unwrap_or(0))),
            preinit_array_addr: get(DT_PREINIT_ARRAY)
                .map(|a| (a, get(DT_PREINIT_ARRAYSZ).unwrap_or(0))),
            fini_array,
            symbols,
            relocs,
            tls,
        })
    }
}

/// A module placed at a concrete base address.
pub struct Module {
    pub image: ModuleImage,
    pub base: u64,
    pub exports: std::collections::HashMap<String, u64>,
    pub unresolved: Vec<String>,
    pub irelative_resolvers: Vec<u64>,
}

impl Module {
    pub fn new(image: ModuleImage, base: u64) -> Module {
        let mut exports = std::collections::HashMap::new();
        for sym in &image.symbols {
            if sym.shndx != 0 && !sym.name.is_empty() {
                exports.entry(sym.name.clone()).or_insert(base + sym.value);
            }
        }
        Module {
            image,
            base,
            exports,
            unresolved: Vec::new(),
            irelative_resolvers: Vec::new(),
        }
    }

    pub fn sym_addr(&self, name: &str) -> Option<u64> {
        self.exports.get(name).copied()
    }
}
