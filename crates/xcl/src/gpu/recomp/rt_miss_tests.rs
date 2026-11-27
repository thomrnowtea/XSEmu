//! Ray tracing off as "every ray misses" (`Options::rt_all_miss`, `XCL_RT_GATE`): an
//! `image_bvh(64)_intersect_ray` reads no node and leaves 0xFFFFFFFF in its four result registers,
//! what a scene without an acceleration structure gives; every other instruction is translated as
//! before. The translated program is compiled by the direct backend (`xcl_dxil`) and run by
//! its interpreter against one that moves -1 into the same registers.

use super::{translate, Mode, Options};
use crate::gpu::rdna2::encode;
use xcl_dxil::interp::{self, Config, Verdict};

type Inst = (&'static str, &'static str, Vec<(&'static str, u64)>);

fn code(insts: &[Inst]) -> Vec<u8> {
    insts.iter().flat_map(|(n, e, f)| encode(n, e, f, None).unwrap_or_else(|x| panic!("{n}: {x}"))).collect()
}

fn intersect(op: &'static str) -> Inst {
    (op, "ENC_MIMG", vec![("VDATA", 100), ("VADDR", 12), ("SRSRC", 2), ("DMASK", 15)])
}

/// `first`, then the four result registers stored at the thread's own offsets.
fn storing(first: Vec<Inst>) -> Vec<u8> {
    let mut p = first;
    p.push(("V_LSHLREV_B32", "ENC_VOP2", vec![("VDST", 44), ("SRC0", 134), ("VSRC1", 0)]));
    for r in 100..104u64 {
        p.push(("BUFFER_STORE_DWORD", "ENC_MUBUF", vec![("VDATA", r), ("VADDR", 44), ("SRSRC", 2), ("SOFFSET", 128), ("OFFSET", 4 * (r - 100)), ("OFFEN", 1)]));
    }
    p.push(("S_ENDPGM", "ENC_SOPP", vec![]));
    code(&p)
}

/// The four result registers set to `src` (an inline constant operand).
fn moves(src: u64) -> Vec<Inst> {
    (100..104u64).map(|r| ("V_MOV_B32", "ENC_VOP1", vec![("VDST", r), ("SRC0", src)])).collect()
}

fn translate_cs(code: &[u8], rt_all_miss: bool) -> String {
    let cs = super::CsInterface { threads: [64, 1, 1], user_sgprs: 4, tgid: [true, false, false], tg_size: false, tid_components: 1 };
    let t = translate(code, &Options { mode: Mode::Compute(cs), wave64: false, split_wave64: true, mem_windows: 4, rt_all_miss, ..Default::default() }).unwrap();
    assert!(t.complete(), "{:?}", t.unsupported_why);
    t.hlsl.replace("vector<uint64_t, 4>(", "uint64_t4(")
}

fn load(hlsl: &str) -> interp::Program {
    interp::load(&xcl_dxil::compile(hlsl, "main", "cs_6_6").unwrap_or_else(|e| panic!("{e}\n{hlsl}"))).unwrap()
}

#[test]
fn every_intersection_is_a_miss_that_reads_no_node() {
    for op in ["IMAGE_BVH_INTERSECT_RAY", "IMAGE_BVH64_INTERSECT_RAY"] {
        for bvh_node_fix in [false, true] {
            let c = code(&[intersect(op), ("S_ENDPGM", "ENC_SOPP", vec![])]);
            let traced = translate(&c, &Options { bvh_node_fix, ..Options::default() }).unwrap();
            assert!(traced.hlsl.contains(" = bvh_intersect("), "{op}");
            let miss = translate(&c, &Options { bvh_node_fix, rt_all_miss: true, ..Options::default() }).unwrap();
            assert!(miss.complete(), "{op}: {:?}", miss.unsupported_why);
            assert!(!miss.hlsl.contains(" = bvh_intersect("), "{op}");
            let line = miss.hlsl.lines().find(|l| l.contains("= 0xFFFFFFFFu;")).unwrap_or_else(|| panic!("{op}: no miss\n{}", miss.hlsl));
            assert!(line.trim_start().starts_with("if ("), "{op}: {line}");
            assert_eq!(line.matches("= 0xFFFFFFFFu;").count(), 4, "{op}: {line}");
        }
    }
}

#[test]
fn a_miss_leaves_what_moving_minus_one_leaves() {
    // operand 193: the inline constant -1; 128: 0
    let minus_one = load(&translate_cs(&storing(moves(193)), true));
    let zero = load(&translate_cs(&storing(moves(128)), true));
    for op in ["IMAGE_BVH_INTERSECT_RAY", "IMAGE_BVH64_INTERSECT_RAY"] {
        let miss = load(&translate_cs(&storing(vec![intersect(op)]), true));
        assert_eq!(interp::diff(&minus_one, &miss, 0..24, &Config::default()).0, Verdict::Equal, "{op}");
        assert_ne!(interp::diff(&zero, &miss, 0..24, &Config::default()).0, Verdict::Equal, "{op}: a result of 0 must show");
    }
}

#[test]
fn a_program_without_an_intersection_is_translated_the_same() {
    let c = storing(moves(193));
    assert_eq!(translate_cs(&c, false), translate_cs(&c, true));
}

#[test]
fn the_gate_is_miss_unless_it_is_hold() {
    for miss in [None, Some(""), Some("miss"), Some("1"), Some("holding")] {
        assert!(super::rt_gate_is_miss(miss), "{miss:?}");
    }
    for hold in ["hold", " HOLD ", "Hold"] {
        assert!(!super::rt_gate_is_miss(Some(hold)), "{hold:?}");
    }
    assert!(!super::rt_all_miss(), "unit tests translate for the structures they build");
}

#[test]
fn a_traversal_translated_as_a_miss_has_its_own_cache_identity() {
    let ident = 0x1234_5678_9abc_def1;
    assert_eq!(super::pso::rt_miss_identity(ident, false), ident);
    assert_ne!(super::pso::rt_miss_identity(ident, true), ident);
    assert_eq!(super::pso::rt_miss_identity(0, true), 0, "a disabled cache stays disabled");
    let traversal = code(&[intersect("IMAGE_BVH64_INTERSECT_RAY"), ("S_ENDPGM", "ENC_SOPP", vec![])]);
    assert!(super::code_traces_rays(&traversal));
    assert!(!super::code_traces_rays(&storing(moves(193))));
}
