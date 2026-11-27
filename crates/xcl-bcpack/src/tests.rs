//! Unit tests, plus a stream packer that drives the real lane schedule so tests can build
//! valid streams for any tables.
//!
//! Differential checks against another decoder run through two ignored tests:
//! `write_cases` writes packed streams to `XCL_BCPACK_CASES`, an outside tool decodes them
//! with the other decoder and writes `<case>.out` (or `<case>.err`) beside each, and
//! `reference_cases` decodes them here and compares.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use super::*;
use crate::tables::{Model, BUCKETS, MODELS, SCALE};

const FORMATS: [Format; 6] = [Format::Bc1, Format::Bc3, Format::Bc4, Format::Bc5, Format::Bc6h, Format::Bc7];

fn mode_of(format: Format) -> u8 {
    match format {
        Format::Bc1 => 1,
        Format::Bc3 => 3,
        Format::Bc4 => 4,
        Format::Bc5 => 5,
        Format::Bc6h => 6,
        Format::Bc7 => 7,
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

/// A table file of our own. `uniform` gives flat models, identity-like ranks and BC7 control
/// code 0 = mode 6; otherwise models, ranks, controls and dictionaries are random (a sixteenth
/// of the controls stay unused).
fn synthetic_tables(seed: u64, uniform: bool) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    let mut t = Vec::with_capacity(TABLES_BYTES);
    for m in 0..MODELS {
        let buckets = m / 16 + 2;
        let mut freq = [0u32; BUCKETS];
        if uniform {
            freq[..buckets].fill(1 << (15 - m / 16));
        } else {
            let mut left = SCALE;
            for b in (1..buckets).rev() {
                let room = (left - 2 - ((1 << (b - 1)) - 1) * 2) >> (b - 1);
                freq[b] = 1 + rng.below(u64::from(room.min(4096)).max(1)) as u32;
                left -= freq[b] << (b - 1);
            }
            freq[0] = left;
        }
        for f in freq {
            t.extend_from_slice(&(f as u16).to_le_bytes());
        }
    }
    if uniform {
        t.extend(0..=255u8);
        t.extend((0..64u8).map(|v| (v & 7) | (v >> 3) << 4));
        t.extend(0..=255u8);
        t.extend((0..64u8).map(|v| (v & 7) | (v >> 3) << 4));
        t.extend((0..64u8).map(|v| (v & 7) | (v >> 3) << 4));
        for code in 0..512u16 {
            let word: u16 = if code == 0 { 6 | 1 << 6 } else { 0 };
            t.extend_from_slice(&word.to_le_bytes());
        }
    } else {
        t.extend((0..256 + 64 + 256 + 64 + 64).map(|_| rng.byte()));
        for _ in 0..512 {
            let word = if rng.below(16) == 0 { 0 } else { random_bc7_control(&mut rng) };
            t.extend_from_slice(&word.to_le_bytes());
        }
    }
    while t.len() < TABLES_BYTES {
        t.push(if uniform { 0 } else { rng.byte() });
    }
    t
}

fn random_bc7_control(rng: &mut Rng) -> u16 {
    let mode = rng.below(8) as u16;
    let subsets = [3, 2, 3, 2, 1, 1, 1, 2][mode as usize];
    let rotation = if mode == 4 || mode == 5 { rng.below(4) as u16 } else { 0 };
    let index_select = if mode == 4 { rng.below(2) as u16 } else { 0 };
    let partition = match subsets {
        1 => 0,
        _ if mode == 0 => rng.below(16) as u16,
        _ => rng.below(64) as u16,
    };
    mode | rotation << 3 | index_select << 5 | subsets << 6 | partition << 8
}

/// rANS-codes `symbols` with `model` as one fragment's payload (state starts and ends at 2^24).
fn encode_payload(model: &Model, symbols: &[u8]) -> Vec<u8> {
    let mut x: u32 = 1 << 24;
    let mut out = Vec::new();
    for &s in symbols.iter().rev() {
        let (freq, cumulative) = if s == 0 {
            (model.freq[0] - 1, 1)
        } else {
            let b = (8 - s.leading_zeros()) as usize;
            (model.freq[b], model.start[b] + (u32::from(s) - (1 << (b - 1))) * model.freq[b])
        };
        while u64::from(x) >= u64::from(freq) << 16 {
            out.push(x as u8);
            x >>= 8;
        }
        x = (x / freq) << 16 | (x % freq + cumulative);
    }
    out.extend_from_slice(&x.to_le_bytes());
    out.reverse();
    out
}

/// One lane's bytes: its symbols split into fragments (every one but the last clears the
/// state), then a filler fragment that leaves the lane parked at 2^24 with a byte held.
fn lane_bytes(t: &Tables, width: u8, symbols: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = symbols;
    while !rest.is_empty() {
        let take = match rng.below(4) {
            0 => rest.len(),
            1 => 1 + rng.below(8) as usize,
            _ => 1 + rng.below(3000) as usize,
        }
        .min(rest.len())
        .min(20_000);
        let (chunk, tail) = rest.split_at(take);
        rest = tail;
        let top = chunk.iter().max().copied().unwrap_or(0);
        let narrowest = (0..=3u8).filter(|&s| s <= width && u32::from(top) < 2u32 << (width - s)).max().unwrap_or(0);
        let shrink = rng.below(u64::from(narrowest) + 1) as u8;
        let shape = rng.below(16) as u8;
        let model = &t.models[usize::from(width - shrink) * 16 + usize::from(shape)];
        let payload = encode_payload(model, chunk);
        let mut header = shrink << 6 | shape;
        if !rest.is_empty() {
            header |= 0x10;
        }
        if payload.len() > 256 {
            let n = payload.len() - 257;
            out.extend_from_slice(&[header | 0x20, n as u8, (n >> 8) as u8]);
        } else {
            out.extend_from_slice(&[header, (payload.len() - 1) as u8]);
        }
        out.extend(payload);
    }
    out.extend_from_slice(&[0, 1, 0, 0]);
    out
}

/// Packs `records` (one per block) into a stream, handing out bytes in the order the
/// schedule asks for them.
fn pack(t: &Tables, format: Format, records: &[Vec<u8>], seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    let per = format.symbols_per_lane();
    let mut queues: Vec<VecDeque<u8>> = format
        .lane_widths()
        .iter()
        .enumerate()
        .map(|(j, &w)| {
            let symbols: Vec<u8> = records.iter().flat_map(|r| r[j * per..(j + 1) * per].to_vec()).collect();
            lane_bytes(t, w, &symbols, &mut rng).into()
        })
        .collect();
    let mut stream = Vec::new();
    let mut check = vec![0; records.len() * format.record_len()];
    lanes::decode(
        t,
        format,
        records.len(),
        |j| {
            let b = queues[j].pop_front().expect("a parked lane asked for a byte");
            stream.push(b);
            Some(b)
        },
        &mut check,
    )
    .unwrap();
    assert_eq!(check, records.concat(), "the schedule hands back what was packed");
    stream
}

/// Random records a valid stream could carry: BC6H modes in range, BC7 codes the tables use.
fn random_records(t: &Tables, format: Format, blocks: usize, rng: &mut Rng) -> Vec<Vec<u8>> {
    let widths = format.lane_widths();
    let per = format.symbols_per_lane();
    let used_codes: Vec<usize> = (0..512).filter(|&c| t.bc7_controls[c] != 0).collect();
    let small = rng.below(2) == 0;
    (0..blocks)
        .map(|_| {
            let mut r: Vec<u8> = (0..format.record_len())
                .map(|k| {
                    let alphabet = 2u64 << widths[k / per];
                    let v = if small && rng.below(4) != 0 { rng.below(4) } else { rng.below(alphabet) };
                    v.min(alphabet - 1) as u8
                })
                .collect();
            match format {
                Format::Bc6h => r[30] = rng.below(14) as u8,
                Format::Bc7 => {
                    let code = used_codes[rng.below(used_codes.len() as u64) as usize];
                    r[30] = (code & 31) as u8;
                    r[31] = (code >> 5) as u8;
                }
                _ => {}
            }
            r
        })
        .collect()
}

fn zigzag(v: i32) -> u8 {
    if v >= 0 {
        (2 * v) as u8
    } else {
        (-2 * v - 1) as u8
    }
}

#[test]
fn table_file_is_validated() {
    assert!(Tables::parse(&synthetic_tables(1, true)).is_ok());
    assert!(Tables::parse(&synthetic_tables(7, false)).is_ok());
    assert_eq!(Tables::parse(&[0; 16]).err(), Some(Error::TableSize(16)));
    let mut bad = synthetic_tables(1, true);
    bad[0] ^= 1;
    assert_eq!(Tables::parse(&bad).err(), Some(Error::TableModel(0)));
    let mut empty_bucket = synthetic_tables(1, true);
    empty_bucket[2..4].fill(0);
    assert_eq!(Tables::parse(&empty_bucket).err(), Some(Error::TableModel(0)));
    assert!(matches!(Tables::load(Path::new("/nonexistent/bcpack-tables.bin")), Err(Error::TableRead(_))));
    assert_eq!(TABLES_BYTES, 77_982);
}

#[test]
fn the_schedule_round_trips_every_format() {
    for (n, uniform) in [true, false].into_iter().enumerate() {
        let t = Tables::parse(&synthetic_tables(11 + n as u64, uniform)).unwrap();
        let mut rng = Rng(99);
        for format in FORMATS {
            for blocks in [1, 2, 37, 700] {
                let records = random_records(&t, format, blocks, &mut rng);
                pack(&t, format, &records, rng.next());
            }
        }
    }
}

#[test]
fn bc1_block_and_back_reference() {
    let t = Tables::parse(&synthetic_tables(1, true)).unwrap();
    // Block 0: endpoints (31, 0, 7) and (1, 63, 0); quad q has indices q, 3 - q, 1, 2.
    let mut b0 = vec![0, 0];
    b0.extend([zigzag(-1), 0, zigzag(7), zigzag(1), zigzag(-1), 0]);
    b0.extend((0..4u8).map(|q| q | (3 - q) << 2 | 1 << 4 | 2 << 6));
    // Block 1 builds on block 0 for both: endpoint 0 red + 1 wraps to 0, indices + 1.
    let mut b1 = vec![zigzag(0), 1];
    b1.extend([zigzag(1), 0, 0, 0, 0, 0]);
    b1.extend([0b01_01_01_01u8; 4]);
    let src = pack(&t, Format::Bc1, &[b0, b1], 5);
    let mut dst = [0u8; 16];
    decode(1, &src, &mut dst, &t).unwrap();
    let block = |c0: u16, c1: u16, ix: &dyn Fn(usize) -> u32| {
        let mut b = [0u8; 8];
        b[..2].copy_from_slice(&c0.to_le_bytes());
        b[2..4].copy_from_slice(&c1.to_le_bytes());
        let sel = (0..16).fold(0u32, |a, p| a | [0, 2, 3, 1][ix(p) as usize] << (2 * p));
        b[4..].copy_from_slice(&sel.to_le_bytes());
        b
    };
    let quad_ix = |p: usize| {
        let q = (p / 8) * 2 + (p % 4) / 2;
        let k = (p / 4 % 2) * 2 + p % 2;
        [q as u32, 3 - q as u32, 1, 2][k]
    };
    assert_eq!(dst[..8], block(31 << 11 | 7, 1 << 11 | 63 << 5, &quad_ix));
    assert_eq!(dst[8..], block(7, 1 << 11 | 63 << 5, &|p| (quad_ix(p) + 1) & 3));
}

#[test]
fn bc4_blocks_with_and_without_references() {
    let t = Tables::parse(&synthetic_tables(1, true)).unwrap();
    let mut b0 = vec![0, 0, zigzag(10), zigzag(-3)];
    b0.extend((0..8).map(|k| ((2 * k) & 7) | ((2 * k + 1) & 7) << 3));
    let mut b1 = vec![0, 1, zigzag(1), zigzag(1)];
    b1.extend((0..8).map(|_| 1 | 1 << 3));
    // Block 2 takes its endpoints from the dictionary (3 back from block 2 is entry 0, all zero)
    // and its indices from block 1.
    let mut b2 = vec![zigzag(2), 1, zigzag(5), zigzag(6)];
    b2.extend([0; 8]);
    let src = pack(&t, Format::Bc4, &[b0, b1, b2], 3);
    let mut dst = [0u8; 24];
    decode(4, &src, &mut dst, &t).unwrap();
    let block = |e0: u8, e1: u8, ix: &dyn Fn(usize) -> usize| {
        let sel = (0..16).fold(0u64, |a, p| a | [0u64, 2, 3, 4, 5, 6, 7, 1][ix(p)] << (16 + 3 * p));
        (u64::from(e0) | u64::from(e1) << 8 | sel).to_le_bytes()
    };
    assert_eq!(dst[..8], block(138, 125, &|p| p & 7));
    assert_eq!(dst[8..16], block(139, 126, &|p| (p + 1) & 7));
    assert_eq!(dst[16..], block(133, 134, &|p| (p + 1) & 7));
}

#[test]
fn bc7_mode6_block() {
    let t = Tables::parse(&synthetic_tables(1, true)).unwrap();
    let mut s = vec![0u8; 32];
    s[4] = zigzag(0x55);
    s[5] = zigzag(-1);
    s[6] = zigzag(3);
    s[10] = zigzag(0x3f);
    let src = pack(&t, Format::Bc7, &[s], 9);
    let mut dst = [0u8; 16];
    decode(7, &src, &mut dst, &t).unwrap();
    // Mode 6: R carries the p-bit as its low bit; R0 0x55 → 0x2a + p 1, R1 0xff → 0x7f + p 1.
    let mut want: u128 = 1 << 6;
    let mut at = 7;
    for v in [0x2a, 0x7f, 3, 0, 0, 0, 0x3f, 0, 1, 1] {
        let n = if at >= 7 + 8 * 7 { 1 } else { 7 };
        want |= (v as u128) << at;
        at += n;
    }
    assert_eq!(dst, want.to_le_bytes());
}

#[test]
fn bad_input_is_an_error() {
    let t = Tables::parse(&synthetic_tables(1, true)).unwrap();
    let records: Vec<Vec<u8>> = (0..64u8).map(|i| vec![0, 0, i, i, 1, 2, 3, 4, 5, 6, 7, 8]).collect();
    let src = pack(&t, Format::Bc4, &records, 1);
    let mut dst = vec![0u8; 64 * 8];
    decode(4, &src, &mut dst, &t).unwrap();
    assert_eq!(decode(4, &src[..src.len() / 2], &mut dst, &t), Err(Error::Truncated));
    assert_eq!(decode(4, &[], &mut dst, &t), Err(Error::Truncated));
    assert_eq!(decode(4, &[[0x30; 8], [0xff; 8]].concat(), &mut dst[..8], &t), Err(Error::Truncated));
    assert_eq!(decode(4, &src, &mut vec![0u8; 65 * 8], &t), Err(Error::Truncated));
    assert_eq!(decode(4, &src, &mut dst[..12], &t), Err(Error::DestinationSize(12)));
    assert_eq!(decode(4, &src, &mut [], &t), Err(Error::DestinationSize(0)));
    for mode in [0, 2, 8, 15] {
        assert_eq!(decode(mode, &src, &mut dst, &t), Err(Error::UnsupportedMode(mode)));
    }
    let mut bc7 = vec![0u8; 32];
    bc7[30] = 1;
    let src = pack(&t, Format::Bc7, &[bc7], 2);
    assert_eq!(decode(7, &src, &mut [0; 16], &t), Err(Error::Bc7Control(1)));
    let mut bc6 = vec![0u8; 32];
    bc6[30] = 14;
    let src = pack(&t, Format::Bc6h, &[bc6], 2);
    assert_eq!(decode(6, &src, &mut [0; 16], &t), Err(Error::Bc6Mode(15)));
}

#[test]
fn noise_never_panics() {
    for uniform in [true, false] {
        let t = Tables::parse(&synthetic_tables(3, uniform)).unwrap();
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for round in 0..300usize {
            let src: Vec<u8> = (0..round * 7 % 900).map(|_| rng.byte()).collect();
            for format in FORMATS {
                let mut dst = vec![0u8; 16 * (1 + round % 40)];
                let _ = decode(mode_of(format), &src, &mut dst, &t);
            }
        }
    }
}

fn real_tables() -> Option<Tables> {
    let path = std::env::var_os("XCL_BCPACK_TABLES")?;
    Some(Tables::load(Path::new(&path)).expect("XCL_BCPACK_TABLES"))
}

#[test]
fn the_real_tables_parse_and_round_trip() {
    let Some(t) = real_tables() else {
        eprintln!("XCL_BCPACK_TABLES not set; skipped");
        return;
    };
    let mut rng = Rng(5);
    for format in FORMATS {
        let records = random_records(&t, format, 300, &mut rng);
        let src = pack(&t, format, &records, rng.next());
        let mut dst = vec![0u8; 300 * format.block_bytes()];
        decode(mode_of(format), &src, &mut dst, &t).unwrap();
    }
}

fn case_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("XCL_BCPACK_CASES").expect("XCL_BCPACK_CASES"))
}

/// Writes `<set>/tables.bin` and packed streams `<set>/<n>-<bytes>.bc<mode>` for a synthetic
/// table set and, when `XCL_BCPACK_TABLES` is set, the real tables (`real/`, no table copy).
/// Some streams are cut short or have a byte flipped.
#[test]
#[ignore = "writes differential cases to XCL_BCPACK_CASES"]
fn write_cases() {
    let root = case_dir();
    let mut sets = vec![("synthetic", synthetic_tables(0x5eed, false))];
    if let Some(path) = std::env::var_os("XCL_BCPACK_TABLES") {
        sets.push(("real", std::fs::read(path).unwrap()));
    }
    let mut rng = Rng(0xc0ffee);
    let mut n = 0;
    for (name, bytes) in sets {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if name != "real" {
            std::fs::write(dir.join("tables.bin"), &bytes).unwrap();
        }
        let t = Tables::parse(&bytes).unwrap();
        for format in FORMATS {
            for k in 0..40 {
                let blocks = match k % 4 {
                    0 => 1 + rng.below(4) as usize,
                    1 => 1 + rng.below(300) as usize,
                    2 => 256 + rng.below(1024) as usize,
                    _ => 2048 + rng.below(4096) as usize,
                };
                let records = random_records(&t, format, blocks, &mut rng);
                let mut src = pack(&t, format, &records, rng.next());
                match rng.below(8) {
                    0 => src.truncate(rng.below(src.len() as u64) as usize),
                    1 => {
                        let at = rng.below(src.len() as u64) as usize;
                        src[at] ^= 1 << rng.below(8);
                    }
                    _ => {}
                }
                let bytes_out = blocks * format.block_bytes();
                std::fs::write(dir.join(format!("{n:05}-{bytes_out}.bc{}", mode_of(format))), &src).unwrap();
                n += 1;
            }
        }
    }
    eprintln!("wrote {n} cases");
}

/// Decodes every case under `XCL_BCPACK_CASES` and compares with the other decoder's
/// `.out` (identical bytes) or `.err` (an error here too).
#[test]
#[ignore = "needs differential cases in XCL_BCPACK_CASES"]
fn reference_cases() {
    let (mut same, mut errors) = (0, 0);
    for set in std::fs::read_dir(case_dir()).unwrap() {
        let dir = set.unwrap().path();
        let bytes = if dir.join("tables.bin").exists() {
            std::fs::read(dir.join("tables.bin")).unwrap()
        } else {
            std::fs::read(std::env::var_os("XCL_BCPACK_TABLES").expect("XCL_BCPACK_TABLES for real/")).unwrap()
        };
        let t = Tables::parse(&bytes).unwrap();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let Some(mode) = path.extension().and_then(|e| e.to_str()?.strip_prefix("bc")?.parse::<u8>().ok()) else {
                continue;
            };
            let stem = path.file_stem().unwrap().to_str().unwrap();
            let size: usize = stem.rsplit('-').next().unwrap().parse().unwrap();
            let mut got = vec![0u8; size];
            let result = decode(mode, &std::fs::read(&path).unwrap(), &mut got, &t);
            if path.with_extension("err").exists() {
                assert!(result.is_err(), "{}: the reference failed, this decoded", path.display());
                errors += 1;
            } else {
                let want = std::fs::read(path.with_extension("out")).expect("reference output");
                assert_eq!(result, Ok(()), "{}", path.display());
                assert!(got == want, "{}: output differs", path.display());
                same += 1;
            }
        }
    }
    eprintln!("{same} identical outputs, {errors} shared errors");
    assert!(same > 0);
}

/// A model's symbol lookup (branchless bucket count, reciprocal for the division) gives what
/// the plain scan and division give, for every slot of every model.
#[test]
fn model_decode_matches_the_plain_lookup() {
    let mut all: Vec<Tables> = [(1, true), (7, false), (23, false)].iter().map(|&(seed, uniform)| Tables::parse(&synthetic_tables(seed, uniform)).unwrap()).collect();
    all.extend(real_tables());
    for t in &all {
        for model in t.models.iter() {
            for slot in 0..SCALE {
                let bucket = model.start[1..].iter().take_while(|&&s| s <= slot).count();
                let (symbol, freq, cumulative) = match bucket {
                    0 => (0, model.freq[0].wrapping_sub(1), 1),
                    b => {
                        let freq = model.freq[b];
                        let offset = (slot - model.start[b]) / freq;
                        ((1 << (b - 1)) + offset, freq, model.start[b] + offset * freq)
                    }
                };
                for high in [1u32 << 8, 0xABCD, 0xFFFF] {
                    let state = high << 16 | slot;
                    let next = (state >> 16).wrapping_mul(freq).wrapping_sub(cumulative).wrapping_add(slot);
                    assert_eq!(model.decode(state), (symbol as u8, next), "slot {slot}");
                }
            }
        }
    }
}

/// Decode speed on synthetic streams, per format (ignored: a measurement, not a check). Run
/// with `cargo test --release -p xcl-bcpack decode_speed -- --ignored --nocapture`.
#[test]
#[ignore]
fn decode_speed() {
    let t = real_tables().unwrap_or_else(|| Tables::parse(&synthetic_tables(5, false)).unwrap());
    let mut rng = Rng(1234);
    for format in FORMATS {
        let blocks = (1 << 20) / format.block_bytes();
        let records = random_records(&t, format, blocks, &mut rng);
        let stream = pack(&t, format, &records, rng.next());
        let mut out = vec![0u8; blocks * format.block_bytes()];
        let mut best = f64::MAX;
        for _ in 0..5 {
            let start = std::time::Instant::now();
            decode(mode_of(format), &stream, &mut out, &t).unwrap();
            best = best.min(start.elapsed().as_secs_f64());
        }
        let mut lanes_best = f64::MAX;
        let mut recs = vec![0u8; blocks * format.record_len()];
        for _ in 0..5 {
            let start = std::time::Instant::now();
            let mut input = stream.iter().copied();
            lanes::decode(&t, format, blocks, |_| input.next(), &mut recs).unwrap();
            lanes_best = lanes_best.min(start.elapsed().as_secs_f64());
        }
        println!(
            "BCPack {format:?}: {} KiB out from {} KiB packed in {:.2} ms (rANS lanes {:.2} ms): {:.0} MB/s of output, {:.1} M blocks/s",
            out.len() >> 10,
            stream.len() >> 10,
            best * 1e3,
            lanes_best * 1e3,
            out.len() as f64 / best / 1e6,
            blocks as f64 / best / 1e6
        );
    }
}
