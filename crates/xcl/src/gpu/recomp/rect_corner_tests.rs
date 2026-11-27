//! The right angle of a console rect (`emit::RECT_CORNER`, the corner `emit::rect_list_gs` completes the
//! rect opposite): the generated GS's own function, taken from its HLSL, is compiled by the
//! direct backend (`xcl_dxil`, which has no geometry stage) into a compute shader and run by
//! its interpreter on corners in every order, off by an ulp, in perspective, rotated, tied and
//! NaN, against the rule: opposite the longest side by squared clip-space x, y distance – v0 if
//! d12 is strictly the longest, v2 if d01 is, else v1 – and none (the one triangle) for a NaN.

use super::emit::{rect_list_gs, NO_RECT_LONGEST};
use xcl_dxil::interp::{self, Config};

fn el(name: &str, reg: u32, sv: u32, ty: u32) -> crate::gpu::dxbc::SigElement {
    crate::gpu::dxbc::SigElement { name: name.into(), index: 0, system_value: sv, component_type: ty, register: reg, mask: 0xF, rw_mask: 0xF, stream: 0 }
}

fn outputs() -> Vec<crate::gpu::dxbc::SigElement> {
    vec![el("SV_Position", 0, 1, 3), el("TEXCOORD", 1, 0, 3), el("COLOR", 2, 0, 1)]
}

/// The rule, on the CPU (f32 like the GPU).
fn rule(q: [[f32; 2]; 3]) -> u32 {
    let side = |i: usize, j: usize| {
        let (dx, dy) = (q[i][0] - q[j][0], q[i][1] - q[j][1]);
        dx * dx + dy * dy
    };
    let (d12, d20, d01) = (side(1, 2), side(2, 0), side(0, 1));
    if d12.is_nan() || d20.is_nan() || d01.is_nan() {
        3
    } else if d12 > d20 && d12 > d01 {
        0
    } else if d01 > d12 && d01 > d20 {
        2
    } else {
        1
    }
}

/// (three corners' clip x, y; the corner they make, where they are a rect's)
fn cases() -> Vec<([[f32; 2]; 3], Option<u32>)> {
    let ulp = |x: f32| f32::from_bits(x.to_bits() + 1);
    let mut v = vec![
        // radeonsi's blits (x1,y1), (x1,y2), (x2,y1): v0; r600 EXA (x1,y1), (x1,y2), (x2,y2): v1
        ([[-1.0, 1.0], [-1.0, -1.0], [1.0, 1.0]], Some(0)),
        ([[-1.0, 1.0], [-1.0, -1.0], [1.0, -1.0]], Some(1)),
        ([[0.25, 0.5], [0.75, 0.75], [0.25, 0.75]], Some(2)),
        // a corner off by an ulp (the exact-equality rule made it the one triangle)
        ([[ulp(-0.5), 0.5], [-0.5, -0.25], [0.375, ulp(0.5)]], Some(0)),
        ([[0.1, 0.2], [ulp(0.1), 0.7], [0.9, ulp(0.7)]], Some(1)),
        // a long thin rect, and one of a pixel
        ([[-1.0, 0.0], [1.0, 0.0], [1.0, 0.001]], Some(1)),
        ([[0.0, 0.0], [0.0, 0.0009765625], [0.00048828125, 0.0]], Some(0)),
        // clip space in perspective: w 2 at v0, 1 elsewhere (x, y are not divided)
        ([[2.0, 2.0], [1.0, -1.0], [-1.0, 1.0]], None),
        // ties: degenerate (all one point, two equal) and a right isosceles' hypotenuse tie
        ([[0.5, 0.5], [0.5, 0.5], [0.5, 0.5]], Some(1)),
        ([[0.0, 0.0], [0.0, 0.0], [1.0, 1.0]], Some(1)),
        ([[0.0, 0.0], [1.0, 0.0], [2.0, 0.0]], Some(1)),
        // NaN and infinity
        ([[f32::NAN, 0.0], [1.0, 0.0], [1.0, 1.0]], Some(3)),
        ([[0.0, 0.0], [1.0, f32::NAN], [1.0, 1.0]], Some(3)),
        ([[f32::INFINITY, 0.0], [f32::INFINITY, 1.0], [0.0, 1.0]], Some(3)),
    ];
    // rotated rects, each corner order: the right angle at v0, v1, v2
    for k in 0..24u32 {
        let (a, s, t) = (k as f32 * 0.27, 0.2 + (k % 5) as f32 * 0.31, 0.1 + (k % 7) as f32 * 0.17);
        let (c, d) = ([0.05 * k as f32 - 0.6, 0.3 - 0.02 * k as f32], [a.cos(), a.sin()]);
        let p = [c[0] + s * d[0], c[1] + s * d[1]];
        let q = [c[0] - t * d[1], c[1] + t * d[0]];
        let order = match k % 3 {
            0 => ([c, p, q], 0),
            1 => ([p, c, q], 1),
            _ => ([p, q, c], 2),
        };
        v.push((order.0, Some(order.1)));
    }
    v
}

fn corners(hlsl: &str, q: &[[[f32; 2]; 3]]) -> Vec<u32> {
    let f = |x: f32| format!("asfloat({:#010x}u)", x.to_bits());
    let at = hlsl.find("uint xcl_rect_corner(").unwrap_or_else(|| panic!("{hlsl}"));
    let end = hlsl.find("[maxvertexcount(").unwrap();
    let mut src = format!("RWByteAddressBuffer xout : register(u0);\n{}[numthreads(1, 1, 1)]\nvoid main()\n{{\n", &hlsl[at..end]);
    for (k, c) in q.iter().enumerate() {
        let p: Vec<String> = c.iter().map(|v| format!("float2({}, {})", f(v[0]), f(v[1]))).collect();
        src.push_str(&format!("    xout.Store({}u, xcl_rect_corner({}));\n", 4 * k, p.join(", ")));
    }
    src.push_str("}\n");
    let p = interp::load(&xcl_dxil::compile(&src, "main", "cs_6_6").unwrap_or_else(|e| panic!("{e}\n{src}"))).unwrap();
    let r = interp::run(&p, 0, &Config::default());
    assert!(r.stop.is_none(), "{:?}\n{src}", r.stop);
    let out: std::collections::HashMap<u64, u32> = r.written.iter().filter(|w| w.0 == 0).map(|w| (w.2, w.3[0])).collect();
    (0..q.len() as u64).map(|k| out[&(4 * k)]).collect()
}

#[test]
fn rect_corner_is_the_right_angle_opposite_the_longest_side() {
    let hlsl = rect_list_gs(&outputs());
    let all = cases();
    let q: Vec<_> = all.iter().map(|c| c.0).collect();
    let got = corners(&hlsl, &q);
    for (k, ((q, want), got)) in all.iter().zip(&got).enumerate() {
        assert_eq!(*got, rule(*q), "case {k}: {q:?}\n{hlsl}");
        if let Some(want) = want {
            assert_eq!(got, want, "case {k}: {q:?}");
        }
    }
}

/// Every corner completes the rect; the earlier rule's GS (`XCL_RECOMP_NO_RECT_LONGEST`) is
/// another text, so it is another compiled GS in the DXIL cache.
#[test]
fn rect_list_gs_completes_every_rect_and_the_earlier_rule_stays_apart() {
    let new = rect_list_gs(&outputs());
    assert!(new.contains("uint c = xcl_rect_corner(v[0].pos.xy, v[1].pos.xy, v[2].pos.xy);") && !new.contains("v[1].pos.w == v[0].pos.w"), "{new}");
    assert!(new.contains("XclOut d = v[0];") && new.contains("d.p0 = a.p0 + b.p0 - k.p0;") && !new.contains("d.p1 ="), "{new}");
    NO_RECT_LONGEST.set(true);
    let old = rect_list_gs(&outputs());
    NO_RECT_LONGEST.set(false);
    assert!(!old.contains("xcl_rect_corner") && old.contains("v[1].pos.w == v[0].pos.w"), "{old}");
    assert_ne!(crate::gpu::shader::fnv64(new.as_bytes()), crate::gpu::shader::fnv64(old.as_bytes()));
}
