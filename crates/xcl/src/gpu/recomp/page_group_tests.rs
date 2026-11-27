//! Page groups (`Em::page_group`): loads at constant offsets from one base read their pages from
//! two resolutions made once for the group (`xcl_group_pages`, `xcl_group_at`) instead of resolving
//! each, against the accessors of each load alone. Both are compiled by the direct backend
//! (`xcl_dxil`) and run by its interpreter: the accessors on addresses at the page and 32-bit
//! ends, with and without a page directory, at every offset a group admits, with lanes whose
//! result the load drops (address 0); and whole translated programs with the groups on and off.
//! As in every translated load, a load's address is the low word of its effective address
//! (`xcl_ea`, aligned down to 4 bytes) and the loads of a group are one effective address plus
//! their offsets.
//! A wrong page, offset or miss must show.

use super::emit::OutlineAtomics;
use super::{translate, Mode, Options};
use crate::gpu::rdna2::encode;
use xcl_dxil::interp::{self, Config, Verdict};

const SETUP: &str = r#"
ByteAddressBuffer xin : register(t0);
RWByteAddressBuffer xout : register(u0);
uint edge(uint x, uint k) {
    uint s = (k >> 28) & 7u;
    return select(s == 0u, 0u, select(s == 1u, x & 15u, select(s == 2u, 65534u + (x & 3u), select(s == 3u, xcl_uma_base + (x & 7u),
        select(s == 4u, 0xfffffff0u + (x & 15u), select(s == 5u, 1u, x))))));
}
uint setup(uint4 a, uint4 b, uint c) {
    xcl_uma_base = select((a.x & 48u) == 0u, 0u, 16u + (a.x & 3u));
    xcl_windows = 1u + ((a.x >> 2) & 3u);
    xcl_mem_views = 100u;
    xcl_page_table = select((c & 3u) == 0u, 0u, (a.y & ~3u) | 4u);
    uint w = select((c & 4u) != 0u, (b.y >> 30) % xcl_windows, edge(b.x ^ a.w, b.z) & 7u);
    xcl_w = w;
    uint k = (c >> 3) & 3u;
    xcl_origin = select(k == 0u, 0u, select(k == 1u, xcl_uma_base + w, select(k == 2u, 0u, edge(a.z, b.x))));
    uint hi = select(k == 0u, 0u, select(k == 1u, xcl_uma_base + w + (b.w & 1u), select(k == 2u, b.w & 1u, edge(b.w, a.y))));
    uint s = (c >> 5) & 3u;
    uint lo = select(s == 0u, (b.z & 0xffff0000u) | (65520u + (b.z & 15u)), select(s == 1u, 0xfffffff0u + (b.z & 15u), select(s == 2u, (b.z & 0xffff0000u) | (b.z & 3u), b.z)));
    xcl_ea = select(and(k == 0u, (b.x & 1u) == 0u), 0ull, ((uint64_t)hi << 32) | (uint64_t)lo);
    return (uint)xcl_ea & ~3u;
}
"#;

const DRIVER: &str = r#"
[numthreads(128, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint i = id.x;
    uint4 a = xin.Load4(i * 32u), b = xin.Load4(i * 32u + 16u);
    uint p = setup(a, b, i ^ (i >> 7));
    uint64_t e0 = xcl_ea;
    uint64_t g = xcl_operand_va(p);
    uint4 r = xcl_group_pages(g);
    uint m = a.w ^ b.y;
    uint o = i * 512u;
    [unroll] for (uint k = 0u; k < 8u; k++) {
        uint d = k == 7u ? 65504u : k == 6u ? 4096u : k == 5u ? 65280u : 4u * k * (k + 1u);
        xcl_ea = e0 + d;
        bool ok = ((m >> k) & 1u) != 0u;
        bool4 q = bool4(((m >> (8u + k)) & 1u) != 0u, ((m >> (16u + k)) & 1u) != 0u, ((m >> (24u + k)) & 1u) == 0u, true);
        uint4 v4 = L4(p + d, g, r);
        uint v1 = select(ok, L1(select(ok, p + d + 4u, 0u), g, r), 0u);
        uint4 x4 = select(q, LX(select(q, (p + d) + uint4(0u, 4u, 8u, 12u), 0u), g, r), 0u);
        xout.Store4(o + 48u * k, v4);
        xout.Store4(o + 48u * k + 16u, x4);
        xout.Store4(o + 48u * k + 32u, uint4(v1, L2(p + d + 8u, g, r), d));
    }
}
"#;

const OLD_CALLS: &str = "uint L1(uint a, uint64_t g, uint4 r) { return xcl_mLoad(a); }\nuint2 L2(uint a, uint64_t g, uint4 r) { return xcl_mLoad2(a); }\nuint4 L4(uint a, uint64_t g, uint4 r) { return xcl_mLoad4(a); }\nuint4 LX(uint4 a, uint64_t g, uint4 r) { return xcl_loadx4(a); }\n";
const NEW_CALLS: &str = "uint L1(uint a, uint64_t g, uint4 r) { return xcl_mLoadG(a, g, r); }\nuint2 L2(uint a, uint64_t g, uint4 r) { return xcl_mLoad2G(a, g, r); }\nuint4 L4(uint a, uint64_t g, uint4 r) { return xcl_mLoad4G(a, g, r); }\nuint4 LX(uint4 a, uint64_t g, uint4 r) { return xcl_loadx4G(a, g, r); }\n";

/// The function of `src` whose text starts with `head`, to its closing brace.
fn function(src: &str, head: &str) -> String {
    let at = src.find(head).unwrap_or_else(|| panic!("{head}"));
    let mut depth = 0;
    for (k, c) in src[at..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' if depth == 1 => return format!("{}\n", &src[at..at + k + 1]),
            '}' => depth -= 1,
            _ => {}
        }
    }
    panic!("{head}: unclosed");
}

/// The accessors of the earlier form, or with the shrunk group accessors (`shrink`).
fn accessors_with(shrink: bool) -> String {
    super::emit::NO_ACCESSOR_SHRINK.set(!shrink);
    let all = super::emit::plain_mem_accessors(4, true);
    super::emit::NO_ACCESSOR_SHRINK.set(false);
    let heads = [
        "uint4 xcl_group_pages(", "uint2 xcl_group_at(", "uint xcl_mLoad(", "uint2 xcl_mLoad2(", "uint4 xcl_mLoad4(", "uint4 xcl_loadx4(",
        "uint xcl_mLoadG(", "uint2 xcl_mLoad2G(", "uint4 xcl_mLoad4G(", "uint4 xcl_loadx4G(",
    ];
    heads.iter().map(|h| function(&all, h)).collect()
}

fn accessors() -> String {
    accessors_with(false)
}

fn program(accessors: &str, calls: &str) -> interp::Program {
    program_with(accessors, calls, DRIVER)
}

fn program_with(accessors: &str, calls: &str, driver: &str) -> interp::Program {
    let src = format!(
        "static uint xcl_mem_views = 0u, xcl_uma_base = 0u, xcl_windows = 0u, xcl_w = 0u;\n{}\n{}\n{SETUP}\n{accessors}\n{calls}\n{driver}",
        super::GPU_PAGE_DIRECTORY_HLSL,
        super::emit::PHYSICAL_ACCESS
    );
    interp::load(&xcl_dxil::compile(&src, "main", "cs_6_6").unwrap_or_else(|e| panic!("{e}\n{src}"))).unwrap()
}

fn verdict(accessors: &str) -> Verdict {
    interp::diff(&program(accessors, OLD_CALLS), &program(accessors, NEW_CALLS), 0..24, &Config::default()).0
}

#[test]
fn grouped_loads_read_what_each_load_alone_reads() {
    for shrink in [false, true] {
        let acc = accessors_with(shrink);
        assert!(acc.contains("xcl_group_at(v3, g, r)") && !acc.contains(" ? "));
        assert_eq!(acc.contains("uint2 z = "), !shrink);
        assert_eq!(acc.contains("[loop] for (uint k = 0u; k < 2u; k++)"), shrink);
        assert_eq!(verdict(&acc), Verdict::Equal, "shrink {shrink}");
    }
}

#[test]
fn a_wrong_page_in_the_shrunk_accessors_is_caught() {
    let acc = accessors_with(true);
    for (from, to) in [
        ("s + 0x10000ull * k + 1ull", "s + 0x10000ull * k"),
        ("k < 2u", "k < 1u"),
        ("select(k == 0u, uint4(p, r.zw)", "select(k == 1u, uint4(p, r.zw)"),
        ("uint2 p1 = xcl_group_at(v1, g, r)", "uint2 p1 = xcl_group_at(v0, g, r)"),
        ("uint2 p2 = xcl_group_at(v2, g, r)", "uint2 p2 = xcl_group_at(v1, g, r)"),
    ] {
        assert!(acc.contains(from), "{from}");
        assert!(matches!(verdict(&acc.replacen(from, to, 1)), Verdict::Different(_)), "mutant {to} not caught");
    }
}

/// `ld_raw` (unaligned: the 20 bytes from the address aligned down) at every byte offset, at the
/// page and 32-bit ends. Its callers pass the low word of the effective address itself, unaligned
/// (or a lane whose result they drop).
const LD_DRIVER: &str = r#"
[numthreads(128, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint i = id.x;
    uint4 a = xin.Load4(i * 32u), b = xin.Load4(i * 32u + 16u);
    setup(a, b, i ^ (i >> 7));
    uint64_t e0 = xcl_ea;
    uint m = a.w ^ b.y;
    uint o = i * 256u;
    [unroll] for (uint k = 0u; k < 8u; k++) {
        uint d = (k == 7u ? 65516u : k == 6u ? 65532u : k == 5u ? 65520u : 4u * k * (k + 1u)) + ((m >> (2u * k)) & 3u);
        xcl_ea = e0 + d;
        xout.Store4(o + 16u * k, ld_raw((uint)xcl_ea));
    }
}
"#;

fn ld_raw(grouped: bool) -> String {
    let f = function(super::emit::PRELUDE_MEM_ACCESS, "uint4 ld_raw(");
    let f = if grouped { f.replacen(super::emit::LD_RAW_LOADS, super::emit::LD_RAW_GROUPED, 1) } else { f };
    f.replace("mem.", "xcl_m")
}

fn ld_verdict(new: &str) -> Verdict {
    let acc = accessors_with(true);
    interp::diff(&program_with(&acc, &ld_raw(false), LD_DRIVER), &program_with(&acc, new, LD_DRIVER), 0..24, &Config::default()).0
}

#[test]
fn ld_raw_in_one_group_reads_what_its_loads_alone_read() {
    let new = ld_raw(true);
    assert!(new.contains("xcl_mLoad4G(b, g, r)") && new.contains("xcl_mLoadG(b + 16u, g, r)"));
    assert_eq!(ld_verdict(&new), Verdict::Equal);
    for (from, to) in [("xcl_operand_va(b)", "xcl_operand_va(b + 16u)"), ("xcl_mLoadG(b + 16u", "xcl_mLoadG(b + 12u")] {
        assert!(matches!(ld_verdict(&new.replacen(from, to, 1)), Verdict::Different(_)), "mutant {to} not caught");
    }
}

#[test]
fn a_wrong_group_page_offset_or_miss_is_caught() {
    let acc = accessors();
    for (from, to) in [
        ("s + 0x10001ull", "s + 0x10000ull"),
        ("y - 1u + ((uint)va & 65535u)", "y + ((uint)va & 65535u)"),
        ("select(k == 1ull, r.z,", "select(k == 2ull, r.z,"),
        ("select(y == 0u, 0u, y - 1u", "select(false, 0u, y - 1u"),
        ("select(a.y == 0u, z.x, q1.x)", "select(a.y != 0u, z.x, q1.x)"),
        ("uint2 z = xcl_resolve_va(xcl_operand_va(0u))", "uint2 z = xcl_resolve_va(xcl_operand_va(4u))"),
        ("uint64_t k = (va - (g & ~0xffffull)) >> 16", "uint64_t k = (va >> 16) - (g >> 16)"),
        ("uint2 p2 = xcl_group_at(xcl_operand_va(a2), g, r)", "uint2 p2 = xcl_group_at(xcl_operand_va(a1), g, r)"),
    ] {
        assert!(acc.contains(from), "{from}");
        assert!(matches!(verdict(&acc.replacen(from, to, 1)), Verdict::Different(_)), "mutant {to} not caught");
    }
}

type Inst = (&'static str, &'static str, Vec<(&'static str, u64)>);

fn code(insts: &[Inst]) -> Vec<u8> {
    insts.iter().flat_map(|(n, e, f)| encode(n, e, f, None).unwrap_or_else(|x| panic!("{n}: {x}"))).collect()
}

/// Constant-offset loads of every grouped kind from one base, a base rewritten between two
/// loads, and an offset past a group's 64 KiB; every loaded register stored.
fn grouped_program() -> Vec<u8> {
    let wait = || ("S_WAITCNT", "ENC_SOPP", vec![("SIMM16", 0)]);
    let buf = |n: &'static str, d: u64, o: u64| -> Inst { (n, "ENC_MUBUF", vec![("VDATA", d), ("VADDR", 40), ("SRSRC", 2), ("SOFFSET", 128), ("OFFSET", o), ("OFFEN", 1)]) };
    let mut p: Vec<Inst> = vec![
        ("S_LOAD_DWORDX4", "ENC_SMEM", vec![("SDATA", 8), ("SBASE", 0), ("OFFSET", 0)]),
        ("S_LOAD_DWORDX4", "ENC_SMEM", vec![("SDATA", 12), ("SBASE", 0), ("OFFSET", 0x40)]),
        ("S_LOAD_DWORDX2", "ENC_SMEM", vec![("SDATA", 16), ("SBASE", 0), ("OFFSET", 0x10010)]),
        wait(),
        ("V_LSHLREV_B32", "ENC_VOP2", vec![("VDST", 40), ("SRC0", 132), ("VSRC1", 0)]),
        buf("BUFFER_LOAD_DWORD", 1, 4),
        buf("BUFFER_LOAD_DWORDX4", 2, 16),
        ("S_BUFFER_LOAD_DWORDX4", "ENC_SMEM", vec![("SDATA", 20), ("SBASE", 4), ("OFFSET", 0x20)]),
        ("S_BUFFER_LOAD_DWORD", "ENC_SMEM", vec![("SDATA", 24), ("SBASE", 4), ("OFFSET", 0x30)]),
        wait(),
        ("V_LSHLREV_B32", "ENC_VOP2", vec![("VDST", 40), ("SRC0", 0xFF), ("VSRC1", 0)]),
        buf("BUFFER_LOAD_DWORDX4", 6, 32),
        ("V_LSHLREV_B32", "ENC_VOP2", vec![("VDST", 42), ("SRC0", 130), ("VSRC1", 0)]),
        ("V_MOV_B32", "ENC_VOP1", vec![("VDST", 43), ("SRC0", 9)]),
        ("GLOBAL_LOAD_DWORDX4", "ENC_FLAT_GLBL", vec![("VDST", 10), ("ADDR", 42), ("SADDR", 0x7D), ("OFFSET", 0)]),
        ("GLOBAL_LOAD_DWORD", "ENC_FLAT_GLBL", vec![("VDST", 14), ("ADDR", 42), ("SADDR", 0x7D), ("OFFSET", 32)]),
        wait(),
    ];
    for (k, s) in [12u64, 13, 16, 17, 20, 23, 24].into_iter().enumerate() {
        p.push(("V_MOV_B32", "ENC_VOP1", vec![("VDST", 15 + k as u64), ("SRC0", s)]));
    }
    p.push(("V_LSHLREV_B32", "ENC_VOP2", vec![("VDST", 44), ("SRC0", 134), ("VSRC1", 0)]));
    for r in 1..22u64 {
        p.push(("BUFFER_STORE_DWORD", "ENC_MUBUF", vec![("VDATA", r), ("VADDR", 44), ("SRSRC", 2), ("SOFFSET", 128), ("OFFSET", 4 * r), ("OFFEN", 1)]));
    }
    p.push(("S_ENDPGM", "ENC_SOPP", vec![]));
    let mut c = code(&p);
    // the literal of the second v_lshlrev (SRC0 0xFF): a voffset 64 KiB away
    let at = c.len() - code(&p[p.iter().position(|i| i.2.contains(&("SRC0", 0xFF))).unwrap()..]).len() + 4;
    c.splice(at..at, 17u32.to_le_bytes());
    c
}

fn translate_cs(code: &[u8], groups: bool) -> String {
    let cs = super::CsInterface { threads: [64, 1, 1], user_sgprs: 4, tgid: [true, false, false], tg_size: false, tid_components: 1 };
    super::emit::NO_PAGE_GROUPS.set(!groups);
    let t = translate(code, &Options { mode: Mode::Compute(cs), wave64: false, split_wave64: true, mem_windows: 4, ..Default::default() });
    super::emit::NO_PAGE_GROUPS.set(false);
    let t = t.unwrap();
    assert!(t.complete(), "{:?}", t.unsupported_why);
    t.hlsl.replace("vector<uint64_t, 4>(", "uint64_t4(")
}

fn load(hlsl: &str) -> interp::Program {
    let hlsl = &observed(hlsl);
    interp::load(&xcl_dxil::compile(hlsl, "main", "cs_6_6").unwrap_or_else(|e| panic!("{e}\n{hlsl}"))).unwrap()
}

/// A translated compute program with the address and value of every dword store and atomic add
/// recorded in a buffer of the test's, in the thread's own words, instead of written to memory.
/// The interpreter's memory is generated: the writes of the programs here resolve to one word
/// of it, where only the last would show, and the waves of a group race on it.
fn observed(hlsl: &str) -> String {
    let store = function(hlsl, "void xcl_mStore(uint a, uint v)");
    let entry = hlsl.find("void main(").unwrap();
    let body = entry + hlsl[entry..].find('{').unwrap() + 1;
    assert!(hlsl[entry..body].contains("uint gi : SV_GroupIndex") && !hlsl.contains("register(u2)"));
    format!("RWByteAddressBuffer xcl_seen : register(u2);\nstatic uint xcl_seen_at = 0u;\n{} xcl_seen_at = gi * 512u;{}", &hlsl[..body], &hlsl[body..])
        .replacen(store.trim_end(), "void xcl_mStore(uint a, uint v) { xcl_seen.Store2(xcl_seen_at, uint2(a, v)); xcl_seen_at += 8u; }", 1)
        .replacen(function(hlsl, "void xcl_vInterlockedAdd(uint a, uint v, out uint o)").trim_end(), "void xcl_vInterlockedAdd(uint a, uint v, out uint o) { xcl_seen.Store2(xcl_seen_at, uint2(a, v)); xcl_seen_at += 8u; o = 0u; }", 1)
}

#[test]
fn translated_loads_with_page_groups_compute_the_same() {
    let c = grouped_program();
    let (old, new) = (translate_cs(&c, false), translate_cs(&c, true));
    assert!(!old.contains("xcl_group_pages(") && !old.contains("G("));
    // the s_load pair, buffer loads at 4 and 16, the s_buffer pair, the x4 after the voffset
    // moved (its own group), the global pair; the s_load past 64 KiB stays alone
    let decls = new.lines().filter(|l| l.contains("= xcl_group_pages(t")).count();
    assert_eq!(decls, 5, "{new}");
    assert!(new.contains("xcl_loadx4G(") && new.contains("xcl_mLoad4G(") && new.contains("xcl_mLoadG(") && new.contains("xcl_vLoadG("));
    assert_eq!(interp::diff(&load(&old), &load(&new), 0..24, &Config::default()).0, Verdict::Equal);
}

/// Two groups whose loads interleave: the first is declared (its earlier calls rewritten to
/// longer text) after the second was opened, so the second's saved position must move with the
/// rewrite, or its declaration lands inside an earlier line.
fn interleaved_program() -> Vec<u8> {
    let wait = || ("S_WAITCNT", "ENC_SOPP", vec![("SIMM16", 0)]);
    let buf = |n: &'static str, d: u64, o: u64| -> Inst { (n, "ENC_MUBUF", vec![("VDATA", d), ("VADDR", 40), ("SRSRC", 4), ("SOFFSET", 128), ("OFFSET", o), ("OFFEN", 1)]) };
    let mut p: Vec<Inst> = vec![
        ("S_LOAD_DWORDX4", "ENC_SMEM", vec![("SDATA", 16), ("SBASE", 0), ("OFFSET", 0x80)]),
        wait(),
        ("V_LSHLREV_B32", "ENC_VOP2", vec![("VDST", 40), ("SRC0", 132), ("VSRC1", 0)]),
        ("S_LOAD_DWORDX4", "ENC_SMEM", vec![("SDATA", 8), ("SBASE", 0), ("OFFSET", 0)]),
        buf("BUFFER_LOAD_DWORD", 1, 4),
        ("S_LOAD_DWORDX4", "ENC_SMEM", vec![("SDATA", 12), ("SBASE", 0), ("OFFSET", 0x40)]),
        buf("BUFFER_LOAD_DWORD", 2, 8),
        buf("BUFFER_LOAD_DWORDX2", 3, 24),
        ("S_LOAD_DWORD", "ENC_SMEM", vec![("SDATA", 20), ("SBASE", 0), ("OFFSET", 0x50)]),
        wait(),
    ];
    for (k, s) in [8u64, 9, 12, 15, 20].into_iter().enumerate() {
        p.push(("V_MOV_B32", "ENC_VOP1", vec![("VDST", 10 + k as u64), ("SRC0", s)]));
    }
    p.push(("V_LSHLREV_B32", "ENC_VOP2", vec![("VDST", 44), ("SRC0", 134), ("VSRC1", 0)]));
    for r in 1..15u64 {
        p.push(("BUFFER_STORE_DWORD", "ENC_MUBUF", vec![("VDATA", r), ("VADDR", 44), ("SRSRC", 4), ("SOFFSET", 128), ("OFFSET", 4 * r), ("OFFEN", 1)]));
    }
    p.push(("S_ENDPGM", "ENC_SOPP", vec![]));
    code(&p)
}

#[test]
fn interleaved_groups_declare_on_their_own_lines() {
    let c = interleaved_program();
    let (old, new) = (translate_cs(&c, false), translate_cs(&c, true));
    let decls: Vec<&str> = new.lines().filter(|l| l.contains("= xcl_group_pages(t")).collect();
    assert_eq!(decls.len(), 2, "{new}");
    for l in &decls {
        assert!(l.trim_start().starts_with("uint64_t t") && l.trim_end().ends_with("_g);"), "a declaration inside another line: {l}");
    }
    assert_eq!(interp::diff(&load(&old), &load(&new), 0..24, &Config::default()).0, Verdict::Equal);
}


/// Byte, short and format loads (`ld_raw`) at unaligned offsets, by buffer and by global
/// address; every loaded register stored.
fn small_loads_program() -> Vec<u8> {
    let wait = || ("S_WAITCNT", "ENC_SOPP", vec![("SIMM16", 0)]);
    let buf = |n: &'static str, d: u64, o: u64| -> Inst { (n, "ENC_MUBUF", vec![("VDATA", d), ("VADDR", 40), ("SRSRC", 4), ("SOFFSET", 128), ("OFFSET", o), ("OFFEN", 1)]) };
    let glbl = |n: &'static str, d: u64, o: u64| -> Inst { (n, "ENC_FLAT_GLBL", vec![("VDST", d), ("ADDR", 42), ("SADDR", 0x7D), ("OFFSET", o)]) };
    let mut p: Vec<Inst> = vec![
        ("S_LOAD_DWORDX4", "ENC_SMEM", vec![("SDATA", 16), ("SBASE", 0), ("OFFSET", 0x80)]),
        wait(),
        ("V_LSHLREV_B32", "ENC_VOP2", vec![("VDST", 40), ("SRC0", 129), ("VSRC1", 0)]),
        ("V_LSHLREV_B32", "ENC_VOP2", vec![("VDST", 42), ("SRC0", 129), ("VSRC1", 0)]),
        ("V_MOV_B32", "ENC_VOP1", vec![("VDST", 43), ("SRC0", 17)]),
        buf("BUFFER_LOAD_UBYTE", 1, 1),
        buf("BUFFER_LOAD_SBYTE", 2, 3),
        buf("BUFFER_LOAD_USHORT", 3, 6),
        buf("BUFFER_LOAD_SSHORT", 4, 4093),
        glbl("GLOBAL_LOAD_UBYTE", 5, 3),
        glbl("GLOBAL_LOAD_SBYTE", 6, 2),
        glbl("GLOBAL_LOAD_USHORT", 7, 1),
        glbl("GLOBAL_LOAD_SSHORT", 8, 4095),
        buf("BUFFER_LOAD_FORMAT_XYZW", 9, 2),
        ("TBUFFER_LOAD_FORMAT_XYZW", "ENC_MTBUF", vec![("VDATA", 13), ("VADDR", 40), ("SRSRC", 4), ("SOFFSET", 128), ("OFFSET", 9), ("OFFEN", 1), ("FORMAT", 29)]),
        ("TBUFFER_LOAD_FORMAT_XY", "ENC_MTBUF", vec![("VDATA", 17), ("VADDR", 40), ("SRSRC", 4), ("SOFFSET", 128), ("OFFSET", 3), ("OFFEN", 1), ("FORMAT", 60)]),
        wait(),
        ("V_LSHLREV_B32", "ENC_VOP2", vec![("VDST", 44), ("SRC0", 134), ("VSRC1", 0)]),
    ];
    for r in 1..19u64 {
        p.push(("BUFFER_STORE_DWORD", "ENC_MUBUF", vec![("VDATA", r), ("VADDR", 44), ("SRSRC", 4), ("SOFFSET", 128), ("OFFSET", 4 * r), ("OFFEN", 1)]));
    }
    p.push(("S_ENDPGM", "ENC_SOPP", vec![]));
    code(&p)
}

fn translate_shrunk(code: &[u8], shrink: bool) -> String {
    super::emit::NO_ACCESSOR_SHRINK.set(!shrink);
    let t = translate_cs(code, true);
    super::emit::NO_ACCESSOR_SHRINK.set(false);
    t
}

#[test]
fn translated_loads_with_shrunk_accessors_compute_the_same() {
    for (c, small) in [(grouped_program(), false), (small_loads_program(), true)] {
        let (old, new) = (translate_shrunk(&c, false), translate_shrunk(&c, true));
        assert!(!old.contains("[loop] for (uint k = 0u; k < 2u; k++)") && !old.contains("xcl_mLoad4G(b, g, r)") && old.contains("uint4 w = xcl_mLoad4(b);"));
        assert!(new.contains("[loop] for (uint k = 0u; k < 2u; k++)") && !new.contains("uint2 z = "));
        assert_eq!(new.contains("uint4 w = xcl_mLoad4G(b, g, r); uint w4 = xcl_mLoadG(b + 16u, g, r);"), small);
        assert_eq!(new.contains("uint4 w = xcl_vLoad4G(b, g, r); uint w4 = xcl_vLoadG(b + 16u, g, r);"), small);
        assert_eq!(interp::diff(&load(&old), &load(&new), 0..24, &Config::default()).0, Verdict::Equal);
    }
}

fn translate_outlined(code: &[u8], outline: bool) -> String {
    translate_form(code, outline, false, OutlineAtomics::All)
}

/// `code` with its memory functions out of line in one form: results through statics or `out`
/// parameters (`emit::outline_statics`), and which atomics are kept (`OutlineAtomics`).
fn translate_form(code: &[u8], outline: bool, statics: bool, atomics: OutlineAtomics) -> String {
    super::emit::OUTLINE.set(Some(outline));
    super::emit::OUTLINE_FORM.set(Some((statics, atomics)));
    let t = translate_cs(code, true);
    super::emit::OUTLINE.set(None);
    super::emit::OUTLINE_FORM.set(None);
    t
}

/// The accessors kept out of line (`emit::outlined`): the same functions behind `[noinline]`
/// functions of scalar parameters. The direct backend inlines them again, so its programs show
/// that the outlined text computes what the plain text does.
#[test]
fn outlined_accessors_compute_the_same() {
    let plain = accessors_with(true);
    let pick = |n: &str| n.starts_with("xcl_m") || n.starts_with("xcl_loadx4") || n == "xcl_group_pages";
    let outlined = |statics: bool, flat: bool| {
        super::emit::OUTLINE_FORM.set(Some((statics, OutlineAtomics::All)));
        super::emit::OUTLINE_FLAT.set(Some(flat));
        let o = super::emit::outlined(&plain, &pick);
        super::emit::OUTLINE_FORM.set(None);
        super::emit::OUTLINE_FLAT.set(None);
        o
    };
    let out = outlined(false, false);
    for f in ["xcl_group_pages", "xcl_mLoad", "xcl_mLoad2", "xcl_mLoad4", "xcl_loadx4", "xcl_mLoadG", "xcl_mLoad2G", "xcl_mLoad4G", "xcl_loadx4G"] {
        assert!(out.contains(&format!("[noinline] void {f}_n(")) && out.contains(&format!(" {f}_i(")), "{f}\n{out}");
    }
    assert!(out.contains("[noinline] void xcl_mLoad4G_n(uint a, uint64_t g, uint r0, uint r1, uint r2, uint r3, out uint xr0, out uint xr1, out uint xr2, out uint xr3) { uint4 xr = xcl_mLoad4G_i(a, g, uint4(r0, r1, r2, r3)); xr0 = xr.x; xr1 = xr.y; xr2 = xr.z; xr3 = xr.w; }"), "{out}");
    assert!(out.contains("uint4 xcl_mLoad4G(uint a, uint64_t g, uint4 r) { uint xr0, xr1, xr2, xr3; xcl_mLoad4G_n(a, g, r.x, r.y, r.z, r.w, xr0, xr1, xr2, xr3); return uint4(xr0, xr1, xr2, xr3); }"), "{out}");
    let diff = |a: &str, b: &str| interp::diff(&program(a, NEW_CALLS), &program(b, NEW_CALLS), 0..24, &Config::default()).0;
    assert_eq!(diff(&plain, &out), Verdict::Equal);
    for (from, to) in [
        ("xr1 = xr.y; xr2 = xr.z; xr3 = xr.w; }\nuint4 xcl_mLoad4G(", "xr1 = xr.x; xr2 = xr.z; xr3 = xr.w; }\nuint4 xcl_mLoad4G("),
        ("xcl_loadx4G_n(a.x, a.y, a.z, a.w, g,", "xcl_loadx4G_n(a.x, a.y, a.w, a.z, g,"),
        ("xcl_group_pages_n(g, xr0, xr1, xr2, xr3); return uint4(xr0, xr1, xr2, xr3)", "xcl_group_pages_n(g, xr0, xr1, xr2, xr3); return uint4(xr2, xr3, xr0, xr1)"),
    ] {
        assert!(out.contains(from), "{from}\n{out}");
        assert!(matches!(diff(&plain, &out.replacen(from, to, 1)), Verdict::Different(_)), "mutant {to} not caught");
    }
    // results through statics: no kept function takes an `out` parameter, and the values are the same
    let st = format!("{}{}", super::emit::OUTLINE_RESULT_STATICS, outlined(true, false));
    assert!(st.lines().filter(|l| l.starts_with("[noinline] ")).all(|l| !l.split_once(')').unwrap().0.contains("out ")), "{st}");
    for f in ["xcl_group_pages", "xcl_mLoad", "xcl_mLoad2", "xcl_mLoad4", "xcl_loadx4", "xcl_mLoadG", "xcl_mLoad2G", "xcl_mLoad4G", "xcl_loadx4G"] {
        assert!(st.contains(&format!("[noinline] void {f}_r(")) && st.contains(&format!(" {f}_i(")), "{f}\n{st}");
    }
    assert!(st.contains("[noinline] void xcl_mLoad4G_r(uint a, uint64_t g, uint r0, uint r1, uint r2, uint r3) { uint xr0; uint xr1; uint xr2; uint xr3; uint4 xr = xcl_mLoad4G_i(a, g, uint4(r0, r1, r2, r3)); xr0 = xr.x; xr1 = xr.y; xr2 = xr.z; xr3 = xr.w; xcl_ret_uint_0 = xr0; xcl_ret_uint_1 = xr1; xcl_ret_uint_2 = xr2; xcl_ret_uint_3 = xr3; }"), "{st}");
    assert!(st.contains("uint4 xcl_mLoad4G(uint a, uint64_t g, uint4 r) { uint xr0, xr1, xr2, xr3; xcl_mLoad4G_r(a, g, r.x, r.y, r.z, r.w); xr0 = xcl_ret_uint_0; xr1 = xcl_ret_uint_1; xr2 = xcl_ret_uint_2; xr3 = xcl_ret_uint_3; return uint4(xr0, xr1, xr2, xr3); }"), "{st}");
    assert_eq!(diff(&plain, &st), Verdict::Equal);
    for (from, to) in [
        ("xcl_ret_uint_1 = xr1; xcl_ret_uint_2 = xr2; xcl_ret_uint_3 = xr3; }\nuint4 xcl_mLoad4G(", "xcl_ret_uint_1 = xr0; xcl_ret_uint_2 = xr2; xcl_ret_uint_3 = xr3; }\nuint4 xcl_mLoad4G("),
        ("xcl_loadx4G_r(a.x, a.y, a.z, a.w, g,", "xcl_loadx4G_r(a.x, a.y, a.w, a.z, g,"),
        ("xcl_group_pages_r(g); xr0 = xcl_ret_uint_0; xr1 = xcl_ret_uint_1;", "xcl_group_pages_r(g); xr0 = xcl_ret_uint_1; xr1 = xcl_ret_uint_0;"),
    ] {
        assert!(st.contains(from), "{from}\n{st}");
        assert!(matches!(diff(&plain, &st.replacen(from, to, 1)), Verdict::Different(_)), "mutant {to} not caught");
    }
    // flat: a kept function calls the text of the kept functions it uses, and the values are the same
    // (the calls themselves run in tests::gpu_outlined_calls_atomics_and_loops_compute_the_same)
    let flat = outlined(false, true);
    let nested = "return uint4(xcl_mLoad(a.x), xcl_mLoad(a.y), xcl_mLoad(a.z), xcl_mLoad(a.w)); }";
    assert!(out.contains(nested) && !flat.contains(nested) && flat.contains(&nested.replace("xcl_mLoad(", "xcl_mLoad_i(")), "{flat}");
    for f in ["xcl_group_pages", "xcl_mLoad", "xcl_mLoad2", "xcl_mLoad4", "xcl_loadx4", "xcl_mLoadG", "xcl_mLoad2G", "xcl_mLoad4G", "xcl_loadx4G"] {
        // what is left of a kept function's name with "(": its own definition
        assert_eq!(flat.matches(&format!(" {f}(")).count() + flat.matches(&format!("({f}(")).count(), 1, "{f}\n{flat}");
    }
    assert_eq!(diff(&plain, &flat), Verdict::Equal);
}

#[test]
fn translated_programs_with_outlined_accessors_compute_the_same() {
    for c in [grouped_program(), small_loads_program(), interleaved_program()] {
        let (plain, out) = (translate_outlined(&c, false), translate_outlined(&c, true));
        assert!(!plain.contains("[noinline]"));
        for f in ["xcl_mLoad", "xcl_vLoad", "xcl_mStore", "xcl_vStore", "xcl_mInterlockedAnd", "xcl_mInterlockedCompareExchange64", "xcl_loadx4", "ld_raw", "ld_raw_v", "st_bytes"] {
            assert!(out.contains(&format!("[noinline] void {f}_n(")), "{f}");
        }
        assert_eq!(interp::diff(&load(&plain), &load(&out), 0..24, &Config::default()).0, Verdict::Equal);
        // the other forms: results through statics, and the atomics inline again but for the adds, or all
        for (statics, atomics) in [(true, OutlineAtomics::All), (false, OutlineAtomics::Add), (true, OutlineAtomics::Add), (true, OutlineAtomics::None)] {
            let form = translate_form(&c, true, statics, atomics);
            assert_eq!(form.contains("static uint xcl_ret_uint_0;"), statics, "{statics} {atomics:?}");
            assert_eq!(form.contains("[noinline] void xcl_mLoad_r(uint a) {"), statics, "{statics} {atomics:?}");
            assert_eq!(form.contains("[noinline] void xcl_mLoad_n(uint a, out uint xr0) {"), !statics, "{statics} {atomics:?}");
            assert!(form.contains("[noinline] void xcl_mStore_n(") && form.contains("[noinline] void st_bytes_n("), "{statics} {atomics:?}");
            assert_eq!(form.contains("[noinline] void xcl_mInterlockedAdd_"), atomics != OutlineAtomics::None, "{statics} {atomics:?}");
            assert_eq!(form.contains("[noinline] void xcl_mInterlockedAnd_"), atomics == OutlineAtomics::All, "{statics} {atomics:?}");
            assert_eq!(form.contains("[noinline] void xcl_mInterlockedCompareExchange64_"), atomics == OutlineAtomics::All, "{statics} {atomics:?}");
            if statics {
                assert!(form.lines().filter(|l| l.starts_with("[noinline] ")).all(|l| !l.split_once(')').unwrap().0.contains("out ")), "{atomics:?}\n{form}");
            }
            assert_eq!(interp::diff(&load(&plain), &load(&form), 0..24, &Config::default()).0, Verdict::Equal, "{statics} {atomics:?}");
        }
    }
}

/// DXC keeps the outlined functions and its validator accepts the container (with a DXC to find:
/// `XCL_DXC_DIR`), and the container is smaller than the plain program's.
#[test]
fn dxc_keeps_outlined_functions() {
    if std::env::var_os("XCL_DXC_DIR").is_none() {
        println!("XCL_DXC_DIR not set");
        return;
    }
    let c = grouped_program();
    let cs = super::CsInterface { threads: [64, 1, 1], user_sgprs: 4, tgid: [true, false, false], tg_size: false, tid_components: 1 };
    let hlsl = |outline: bool| {
        super::emit::OUTLINE.set(Some(outline));
        let t = translate(&c, &Options { mode: Mode::Compute(cs.clone()), wave64: false, split_wave64: true, mem_windows: 4, ..Default::default() });
        super::emit::OUTLINE.set(None);
        t.unwrap().hlsl
    };
    let size = |src: &str| super::dxc::compile_dxc_only(src, "main", "cs_6_6").unwrap_or_else(|e| panic!("{e}")).len();
    super::emit::OUTLINE_FORM.set(Some((false, OutlineAtomics::All)));
    let (plain, out) = (size(&hlsl(false)), size(&hlsl(true)));
    super::emit::OUTLINE_FORM.set(None);
    println!("plain {plain} B, outlined {out} B");
    assert!(out < plain);
    // the other forms too: results through statics, and the atomics inline but for the adds
    for (statics, atomics) in [(true, OutlineAtomics::All), (false, OutlineAtomics::Add), (true, OutlineAtomics::Add)] {
        super::emit::OUTLINE_FORM.set(Some((statics, atomics)));
        let form = size(&hlsl(true));
        super::emit::OUTLINE_FORM.set(None);
        println!("outlined, results through statics {statics}, atomics {atomics:?}: {form} B");
        assert!(form < plain);
    }
}

fn translate_plain(code: &[u8], plain: bool) -> String {
    super::emit::PLAIN_LOADS.set(Some(plain));
    let t = translate_cs(code, true);
    super::emit::PLAIN_LOADS.set(None);
    t
}

/// `c` with an atomic add before its end: a coherent program, its loads before its first write.
/// Each thread adds to a word of its own that no load of the program reads (two waves race in
/// the interpreter when one writes what the other reads).
fn with_atomic(mut c: Vec<u8>) -> Vec<u8> {
    let end = code(&[("S_ENDPGM", "ENC_SOPP", vec![])]);
    assert!(c.ends_with(&end));
    c.truncate(c.len() - end.len());
    c.extend(code(&[
        ("V_LSHLREV_B32", "ENC_VOP2", vec![("VDST", 47), ("SRC0", 131), ("VSRC1", 0)]),
        ("V_MOV_B32", "ENC_VOP1", vec![("VDST", 48), ("SRC0", 9)]),
        ("GLOBAL_ATOMIC_ADD", "ENC_FLAT_GLBL", vec![("VDST", 46), ("ADDR", 47), ("DATA", 1), ("SADDR", 0x7D), ("OFFSET", 0x800), ("GLC", 1)]),
        ("S_ENDPGM", "ENC_SOPP", vec![]),
    ]));
    c
}

/// A coherent program's loads before its first write on the view that is not coherent
/// (`emit::plain_load_limit`): the accessors they call are the coherent ones without the qualifier, and
/// the program computes what it computes with every load on the coherent view. The interpreter has
/// one memory behind both views, so this shows the addresses, pages and groups of the plain
/// accessors and that the program stores what they return; what the GPU caches it cannot show.
#[test]
fn plain_loads_compute_the_same() {
    let mut mutants = std::collections::BTreeSet::new();
    for c in [with_atomic(grouped_program()), with_atomic(small_loads_program()), with_atomic(interleaved_program())] {
        let (coherent, plain) = (translate_plain(&c, false), translate_plain(&c, true));
        assert!(coherent.contains("globallycoherent ") && !coherent.contains("_cached"));
        assert!(plain.contains(" = xcl_m_cachedLoad4") && plain.contains("xcl_mStore(") && plain != coherent);
        assert_eq!(plain, translate_cs(&c, true), "on unless switched off");
        let reference = load(&coherent);
        // no race and nothing undefined in the program itself
        assert!((0..24).all(|seed| interp::run(&reference, seed, &Config::default()).ub == 0));
        assert_eq!(interp::diff(&reference, &load(&plain), 0..24, &Config::default()).0, Verdict::Equal);
        for (name, cached) in [("xcl_mLoad", "xcl_m_cachedLoad"), ("xcl_mLoad2", "xcl_m_cachedLoad2"), ("xcl_mLoad4", "xcl_m_cachedLoad4"), ("xcl_mLoadG", "xcl_m_cachedLoadG"), ("xcl_mLoad2G", "xcl_m_cachedLoad2G"), ("xcl_mLoad4G", "xcl_m_cachedLoad4G"), ("xcl_loadx4", "xcl_loadx4_cached"), ("xcl_loadx4G", "xcl_loadx4G_cached")] {
            let ret = if name.contains('2') { "uint2" } else if name.contains('4') { "uint4" } else { "uint" };
            let (f, p) = (function(&plain, &format!("{ret} {name}(")), function(&plain, &format!("{ret} {cached}(")));
            assert!(f.contains("globallycoherent RWByteAddressBuffer") && !p.contains("globallycoherent"), "{name}");
            assert_eq!(f.replacen(&format!("{name}("), &format!("{cached}("), 1).replace("globallycoherent ", "").replace("xcl_mLoad(", "xcl_m_cachedLoad("), p, "{name}");
            // the program stores what each plain accessor it calls returns
            let body = &plain[plain.find("void main(").unwrap()..];
            if !body.contains(&format!(" {cached}(")) {
                continue;
            }
            let head = &p[..p.find('{').unwrap() + 1];
            let wrong = match ret { "uint" => "77u", "uint2" => "uint2(77u, 78u)", _ => "uint4(77u, 78u, 79u, 80u)" };
            let mutant = plain.replacen(head, &format!("{head} return {wrong};"), 1);
            assert!(mutant != plain);
            let v = interp::diff(&reference, &load(&mutant), 0..24, &Config::default()).0;
            assert!(matches!(v, Verdict::Different(_)), "{cached} returning a constant: {v:?}");
            mutants.insert(cached);
        }
    }
    assert_eq!(mutants.into_iter().collect::<Vec<_>>(), ["xcl_loadx4G_cached", "xcl_m_cachedLoad2", "xcl_m_cachedLoad4", "xcl_m_cachedLoad4G", "xcl_m_cachedLoadG"]);
    // a wrong address in a plain accessor shows
    let plain = translate_plain(&with_atomic(grouped_program()), true);
    let coherent = load(&translate_plain(&with_atomic(grouped_program()), false));
    for (cached, from, to) in [
        ("uint4 xcl_m_cachedLoad4G(", "return xcl_mb.Load4(a);", "return xcl_mb.Load4(a + 4u);"),
        ("uint xcl_m_cachedLoadG(", "return xcl_mb.Load(a);", "return xcl_mb.Load(a + 4u);"),
    ] {
        let at = plain.find(cached).unwrap();
        assert!(plain[at..].contains(from), "{cached} {from}");
        let mutant = format!("{}{}", &plain[..at], plain[at..].replacen(from, to, 1));
        let v = interp::diff(&coherent, &load(&mutant), 0..24, &Config::default()).0;
        assert!(matches!(v, Verdict::Different(_)), "{cached} {to}: {v:?}");
    }
}
