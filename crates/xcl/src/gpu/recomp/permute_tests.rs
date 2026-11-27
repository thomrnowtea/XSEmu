//! The cross-lane reads of a lane outside EXEC, which on the console gives 0 (or, for DPP16
//! without bound_ctrl, leaves the reader unwritten) – RDNA2 ISA: "if src_lane selects a disabled
//! thread, zero is returned" (ds_bpermute_b32); ds_permute_b32 writes from the lanes in EXEC
//! only and a lane nobody writes reads 0; DPP16 with FI clear takes a disabled source lane as
//! invalid like one out of its row (bound_ctrl: 0, else no write); DPP8 with FI clear reads 0
//! from it (LLVM AMDGPU, Mesa ACO). Kernel programs are translated as wave32, wave64 and split
//! wave64, compiled by the direct backend (`xcl_dxil`) and run by its interpreter with every lane's
//! data register non-zero and EXEC half set; each lane's result must be the console's. A form
//! that read the disabled lane's register must show (the harness is not blind to it).

use std::collections::HashMap;

use super::{translate, Mode, Options, KERNEL_SGPRS, KERNEL_VGPRS};
use crate::gpu::rdna2::encode;
use xcl_dxil::interp::{self, res, Config, Input};

type Inst = (&'static str, &'static str, Vec<(&'static str, u64)>);

const EXEC: u64 = 0x0F0F_33CC_6DB6_DB6D;
/// v1: the data, v2: the address, v3: the result (its old value on lanes not written)
const DATA: u64 = 1;
const ADDR: u64 = 2;
const DST: u64 = 3;

fn data(l: u32) -> u32 {
    0x1000 + 0x11 * l
}

fn old(l: u32) -> u32 {
    0xDEAD_0000 | l
}

/// lane l's address: lanes spread over the wave, in and outside EXEC
fn addr(l: u32) -> u32 {
    ((l * 5 + 3) & 63) * 4
}

fn on(exec: u64, l: u32) -> bool {
    exec >> l & 1 != 0
}

fn program(ops: &[Inst]) -> Vec<u8> {
    let mut v: Vec<Inst> = ops.to_vec();
    v.push(("S_WAITCNT", "ENC_SOPP", vec![("SIMM16", 0)]));
    v.push(("S_ENDPGM", "ENC_SOPP", vec![]));
    v.iter().flat_map(|(n, e, f)| encode(n, e, f, None).unwrap_or_else(|x| panic!("{n}: {x}"))).collect()
}

fn bpermute() -> Inst {
    ("DS_BPERMUTE_B32", "ENC_DS", vec![("VDST", DST), ("ADDR", ADDR), ("DATA0", DATA), ("OFFSET0", 0), ("OFFSET1", 0)])
}

fn permute() -> Inst {
    ("DS_PERMUTE_B32", "ENC_DS", vec![("VDST", DST), ("ADDR", ADDR), ("DATA0", DATA), ("OFFSET0", 0), ("OFFSET1", 0)])
}

/// v_mov_b32 v3, v1 row_shr:1
fn row_shr(bound: u64) -> Inst {
    ("V_MOV_B32", "VOP1_VOP_DPP16", vec![("VDST", DST), ("SRC0", 0xFA), ("VSRC0", DATA), ("DPP_CTRL", 0x111), ("ROW_MASK", 0xF), ("BANK_MASK", 0xF), ("BOUND_CTRL", bound)])
}

/// v_mov_b32 v3, v1 dpp8:[7,6,5,4,3,2,1,0]
fn dpp8() -> Inst {
    const F: [&str; 8] = ["LANE_SEL_0", "LANE_SEL_1", "LANE_SEL_2", "LANE_SEL_3", "LANE_SEL_4", "LANE_SEL_5", "LANE_SEL_6", "LANE_SEL_7"];
    let mut f = vec![("VDST", DST), ("SRC0", 0xE9), ("VSRC0", DATA)];
    f.extend(F.iter().zip(0..8u64).map(|(&n, s)| (n, 7 - s)));
    ("V_MOV_B32", "VOP1_VOP_DPP8", f)
}

/// Lane l's v3 on the console after `op` in a wave of `n` lanes.
fn console(op: &str, n: u32, exec: u64) -> Vec<u32> {
    let read = |l: u32, src: Option<u32>, bound: bool| match src.filter(|&s| on(exec, s)) {
        Some(s) => data(s),
        None if bound => 0,
        None => old(l),
    };
    let mut forward = vec![0u32; n as usize];
    for i in (0..n).filter(|&i| on(exec, i)) {
        forward[((addr(i) >> 2) & (n - 1)) as usize] = data(i);
    }
    (0..n)
        .map(|l| {
            if !on(exec, l) {
                return old(l);
            }
            match op {
                "bpermute" => read(l, Some((addr(l) >> 2) & (n - 1)), true),
                "permute" => forward[l as usize],
                "row_shr_bound" => read(l, (l & 15 >= 1).then(|| l - 1), true),
                "row_shr" => read(l, (l & 15 >= 1).then(|| l - 1), false),
                "dpp8" => read(l, Some((l & !7) | (7 - (l & 7))), true),
                _ => unreachable!(),
            }
        })
        .collect()
}

fn translate_kernel(code: &[u8], wave64: bool, split: bool) -> String {
    let t = translate(code, &Options { mode: Mode::Kernel, wave64, split_wave64: split, ..Default::default() }).unwrap();
    assert!(t.complete(), "{:?}", t.unsupported_why);
    t.hlsl
}

/// Each lane's v3 after running `hlsl` (a kernel of `n` lanes) with EXEC `exec`.
fn lanes(hlsl: &str, n: u32, exec: u64) -> Vec<u32> {
    let p = interp::load(&xcl_dxil::compile(hlsl, "main", "cs_6_6").unwrap_or_else(|e| panic!("{e}\n{hlsl}"))).unwrap();
    let mut patches = vec![(126, exec as u32), (127, (exec >> 32) as u32)];
    for l in 0..n {
        let reg = |v: u64| (KERNEL_SGPRS + l as usize * KERNEL_VGPRS) as u64 + v;
        patches.extend([(reg(DATA), data(l)), (reg(ADDR), addr(l)), (reg(DST), old(l))]);
    }
    let input = Input { seed: 0, patches: patches.into_iter().map(|(w, v)| ((res::IDENT_IO as u64) << 32 | w, v)).collect() };
    let (r, _) = interp::run_input(&p, &input, &Config { force_ballots: false, ..Default::default() }, false);
    assert!(r.stop.is_none(), "{:?}\n{hlsl}", r.stop);
    let out: HashMap<u64, u32> = r.written.iter().filter(|w| w.0 == 0 && w.1 == res::IDENT_IO + 1).map(|w| (w.2 / 4, w.3[0])).collect();
    (0..n).map(|l| out[&((KERNEL_SGPRS + l as usize * KERNEL_VGPRS) as u64 + DST)]).collect()
}

fn forms() -> [(bool, bool, u32); 3] {
    [(false, false, 32), (true, false, 64), (true, true, 64)]
}

#[test]
fn cross_lane_reads_of_lanes_outside_exec_are_the_consoles() {
    let cases: [(&str, Inst); 5] = [("bpermute", bpermute()), ("permute", permute()), ("row_shr_bound", row_shr(1)), ("row_shr", row_shr(0)), ("dpp8", dpp8())];
    for (name, op) in cases {
        let code = program(&[op]);
        for (wave64, split, n) in forms() {
            let exec = if n == 32 { EXEC & 0xFFFF_FFFF } else { EXEC };
            let hlsl = translate_kernel(&code, wave64, split);
            assert_eq!(lanes(&hlsl, n, exec), console(name, n, exec), "{name}: wave64 {wave64} split {split}\n{hlsl}");
        }
    }
}

/// Without the EXEC test on the source lane, ds_bpermute reads the disabled lanes' data.
#[test]
fn a_bpermute_reading_disabled_lanes_is_caught() {
    let code = program(&[bpermute()]);
    for (wave64, split, n) in forms() {
        let exec = if n == 32 { EXEC & 0xFFFF_FFFF } else { EXEC };
        let hlsl = translate_kernel(&code, wave64, split);
        let mutant: String = hlsl.lines().filter(|l| !(l.contains("select(lanebit_at(") && l.contains("_l), ") && l.trim_end().ends_with(", 0u);"))).map(|l| format!("{l}\n")).collect();
        assert_ne!(mutant, hlsl);
        assert_ne!(lanes(&mutant, n, exec), console("bpermute", n, exec), "wave64 {wave64} split {split}\n{mutant}");
    }
}
