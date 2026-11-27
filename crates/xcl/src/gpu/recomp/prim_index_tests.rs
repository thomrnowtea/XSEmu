//! Primitive words with 9-bit vertex indices (`emit::prim_indices`, `emit::gs_record_prim`): the
//! unpacking of the mesh pass-through and of a host mesh shader, and the words a GS writes into
//! its record, are taken from the translator's HLSL, compiled by the direct backend (`xcl_dxil`)
//! into a compute shader and run by its interpreter on words with the per-edge flags set (bits
//! 9, 19 and 29) and on GS vertex bases past 512, against the console's layout. The earlier
//! 10-bit fields (`XCL_RECOMP_NO_PRIM9`) must show: they read a flagged index as index + 512.

use std::collections::HashMap;

use super::{translate, GsInterface, Mode, MeshNative, MsInterface, Options};
use crate::gpu::rdna2::encode;
use xcl_dxil::interp::{self, Config};

fn el(name: &str, sv: u32, reg: u32) -> crate::gpu::dxbc::SigElement {
    crate::gpu::dxbc::SigElement { name: name.into(), index: 0, system_value: sv, component_type: 3, register: reg, mask: 0xF, rw_mask: 0xF, stream: 0 }
}

/// The trimmed line of `hlsl` that starts with `start` (the first).
fn line<'a>(hlsl: &'a str, start: &str) -> &'a str {
    hlsl.lines().map(str::trim).find(|l| l.starts_with(start)).unwrap_or_else(|| panic!("no {start}\n{hlsl}"))
}

/// The buffer words a one-thread compute shader with `body` stores to u0, by byte address.
fn run(body: &str) -> HashMap<u64, u32> {
    let hlsl = format!("RWByteAddressBuffer xout : register(u0);\n[numthreads(1, 1, 1)]\nvoid main()\n{{\n{body}}}\n");
    let p = interp::load(&xcl_dxil::compile(&hlsl, "main", "cs_6_6").unwrap_or_else(|e| panic!("{e}\n{hlsl}"))).unwrap();
    let r = interp::run(&p, 0, &Config { force_ballots: false, ..Default::default() });
    assert!(r.stop.is_none(), "{:?}\n{hlsl}", r.stop);
    r.written.iter().filter(|w| w.0 == 0).map(|w| (w.2, w.3[0])).collect()
}

/// What `decode` (the "uint3 x = " and "bool bad = " lines of one form, reading the word `word`
/// against the vertex count `count`) makes of each word: Some(indices), None for a culled one.
fn decoded(decode: [&str; 2], word: &str, count: &str, nv: u32, words: &[u32]) -> Vec<Option<[u32; 3]>> {
    let mut body = String::new();
    for (k, w) in words.iter().enumerate() {
        body.push_str(&format!("    {{\n        uint {word} = {w}u, {count} = {nv}u;\n        {}\n        {}\n        xout.Store4({}u, uint4(x, select(bad, 1u, 0u)));\n    }}\n", decode[0], decode[1], 16 * k));
    }
    let out = run(&body);
    (0..words.len() as u64).map(|k| (out[&(16 * k + 12)] == 0).then(|| [0, 1, 2].map(|c| out[&(16 * k + 4 * c)]))).collect()
}

/// (word, what the console reads): three indices at [8:0], [18:10], [28:20] whatever the edge
/// flags; null_prim (bit 31) and indices at or past the vertex count (8 here) culled.
fn console_words() -> Vec<(u32, Option<[u32; 3]>)> {
    let pack = |a: u32, b: u32, c: u32| a | b << 10 | c << 20;
    let flags = 1 << 9 | 1 << 19 | 1 << 29;
    vec![
        (pack(3, 1, 2), Some([3, 1, 2])),
        (pack(3, 1, 2) | flags, Some([3, 1, 2])),
        (pack(0, 7, 5) | 1 << 19, Some([0, 7, 5])),
        (pack(6, 0, 4) | 1 << 9 | 1 << 29, Some([6, 0, 4])),
        (pack(1, 2, 3) | 1 << 31, None),
        (pack(1, 2, 3) | flags | 1 << 31, None),
        (pack(8, 2, 3), None),
        (pack(255, 2, 3) | flags, None),
    ]
}

fn passthrough_decode(ms: &MsInterface) -> [String; 2] {
    let h = super::mesh_passthrough(ms);
    [line(&h, "uint3 x = ").to_string(), line(&h, "bool bad = ").to_string()]
}

fn native_decode() -> [String; 2] {
    let ms = MsInterface { threads: 32, max_vertices: 8, max_primitives: 8, outputs: vec![el("SV_Position", 1, 0)] };
    let code = encode("S_ENDPGM", "ENC_SOPP", &[], None).unwrap();
    let opt = Options { mode: Mode::Mesh(ms), wave64: false, mesh_native: Some(MeshNative { ps_inputs: None, lds_bytes: 0 }), ..Default::default() };
    let h = translate(&code, &opt).unwrap().hlsl;
    [line(&h, "uint3 x = ").to_string(), line(&h, "bool bad = ").to_string()]
}

#[test]
fn primitive_words_read_nine_bit_indices_past_the_edge_flags() {
    let ms = MsInterface { threads: 64, max_vertices: 8, max_primitives: 8, outputs: vec![el("SV_Position", 1, 0), el("TEXCOORD", 0, 1)] };
    let (words, want): (Vec<u32>, Vec<Option<[u32; 3]>>) = console_words().into_iter().unzip();
    let pass = passthrough_decode(&ms);
    let native = native_decode();
    assert!(pass[0].contains("p & 0x1FFu") && native[0].contains("xcl_op & 0x1FFu"), "{pass:?} {native:?}");
    assert_eq!(decoded([&pass[0], &pass[1]], "p", "nv", 8, &words), want, "{pass:?}");
    assert_eq!(decoded([&native[0], &native[1]], "xcl_op", "xcl_nv", 8, &words), want, "{native:?}");

    // the split pass-through (over the mesh output limit) reads its words alike
    let mut wide = ms.clone();
    wide.outputs.extend((2..18).map(|r| el("TEXCOORD", 0, r)));
    wide.max_vertices = 128;
    wide.max_primitives = 128;
    let ins: Vec<_> = (0..16).map(|k| crate::gpu::dxbc::SigElement { index: k, ..el("TEXCOORD", 0, k + 1) }).collect();
    assert!(super::emit::passthrough_split(&wide, super::emit::passthrough_elements(&wide, Some(&ins))) > 1);
    let split = super::emit::mesh_passthrough_for(&wide, Some(&ins));
    assert_eq!(line(&split, "uint3 x = "), pass[0], "{split}");

    super::emit::NO_PRIM9.set(true);
    let (old_pass, old_native) = (passthrough_decode(&ms), native_decode());
    super::emit::NO_PRIM9.set(false);
    assert!(old_pass[0].contains("p & 0x3FFu") && old_native[0].contains("xcl_op & 0x3FFu"));
    let old = decoded([&old_pass[0], &old_pass[1]], "p", "nv", 8, &words);
    assert_eq!(old[0], want[0]);
    assert_eq!(old[1], None, "a flagged index reads as index + 512, past the vertices");
}

/// A GS alone (`gs_start` 0) of `max_vertex_count` vertices that emits three of them.
fn geometry_hlsl(max_vertex_count: u32) -> (String, u32) {
    let mut code = Vec::new();
    for _ in 0..3 {
        code.extend(encode("S_SENDMSG", "ENC_SOPP", &[("SIMM16", 0x22)], None).unwrap());
    }
    code.extend(encode("S_SENDMSG", "ENC_SOPP", &[("SIMM16", 0x3)], None).unwrap());
    code.extend(encode("S_ENDPGM", "ENC_SOPP", &[], None).unwrap());
    let g = GsInterface { es_inputs: vec![], es_outputs: vec![el("SV_Position", 1, 0)], outputs: vec![el("SV_Position", 1, 0)], max_vertex_count, gs_start: Some(0) };
    let maxv = g.record().max_vertices;
    (translate(&code, &Options { mode: Mode::Geometry(g), wave64: true, split_wave64: false, ..Default::default() }).unwrap().hlsl, maxv)
}

/// The value the GS record store of `hlsl` writes for its triangle `t`.
fn record_word(hlsl: &str) -> String {
    let store = line(hlsl, "res.Store((xcl_rec + ");
    let value = store.split_once("* 4u, ").unwrap_or_else(|| panic!("{store}")).1;
    value.strip_suffix(");").unwrap_or_else(|| panic!("{store}")).to_string()
}

/// A GS record's triangles through the pass-through: strip triangle (k, k+1, k+2) of a lane
/// whose vertices start at vertex base `vb` is that triangle of the record while its vertices
/// fit the record's `maxv` (256 here, so 9 bits hold them) and culled once they do not – also
/// for bases of 512 and more (a GS of 32 vertices in a group of 21 primitives reaches 671),
/// which the 9-bit read takes for other vertices of the record when the word holds 10-bit sums.
#[test]
fn gs_record_words_are_primitive_words_or_null() {
    let (hlsl, maxv) = geometry_hlsl(32);
    assert_eq!(maxv, 256);
    let ms = MsInterface { threads: 64, max_vertices: maxv, max_primitives: 256, outputs: vec![el("SV_Position", 1, 0)] };
    let pass = passthrough_decode(&ms);
    let cases: Vec<(u32, u32)> = vec![(0, 0), (0, 29), (250, 3), (253, 0), (254, 0), (300, 0), (512, 0), (520, 1), (1030, 0)];
    let roundtrip = |value: &str| {
        let mut body = String::new();
        for (k, (vb, base)) in cases.iter().enumerate() {
            let t = base | (base + 1) << 10 | (base + 2) << 20;
            body.push_str(&format!(
                "    {{\n        uint t = {t}u, xcl_vb = {vb}u, nv = {maxv}u;\n        uint p = {value};\n        {}\n        {}\n        xout.Store4({}u, uint4(x, select(bad, 1u, 0u)));\n    }}\n",
                pass[0], pass[1], 16 * k
            ));
        }
        let out = run(&body);
        (0..cases.len() as u64).map(|k| (out[&(16 * k + 12)] == 0).then(|| [0, 1, 2].map(|c| out[&(16 * k + 4 * c)]))).collect::<Vec<_>>()
    };
    let want: Vec<Option<[u32; 3]>> = cases.iter().map(|&(vb, base)| (vb + base + 2 < maxv).then(|| [0, 1, 2].map(|c| vb + base + c))).collect();
    let value = record_word(&hlsl);
    assert!(value.contains("0x80000000u"), "{value}");
    assert_eq!(roundtrip(&value), want, "{value}");

    super::emit::NO_PRIM9.set(true);
    let (old_hlsl, _) = geometry_hlsl(32);
    super::emit::NO_PRIM9.set(false);
    let old_value = record_word(&old_hlsl);
    assert!(!old_value.contains("0x80000000u"), "{old_value}");
    let mixed = roundtrip(&old_value);
    assert_eq!(mixed[6], Some([0, 1, 2]), "10-bit sums at 512 read as the record's first vertices");
}
