//! The format's constant tables, read from the table file at run time.
//!
//! File layout (little-endian, no padding):
//! 1. 128 models × 9 u16 bucket frequencies,
//! 2. index rank tables: BC1 (256 B), BC3 alpha (64 B), BC3 colour (256 B), BC4 (64 B), BC5 (64 B),
//! 3. 512 u16 BC7 control words,
//! 4. 255 dictionary entries per format, in the order BC1, BC3, BC4, BC5, BC6H, BC7. A BC1–BC5
//!    entry is its endpoints then its indices, one byte each (BC1 6 + 16, BC3 8 + 32, BC4
//!    2 + 16, BC5 4 + 32). A BC6H entry is an unused byte, 12 u16 endpoints, a mode byte and
//!    16 i16 indices; a BC7 entry is a u16 control word, 24 u16 endpoints, 2 index widths and
//!    32 i16 indices.

use std::path::Path;

use crate::bc6::{Bc6Endpoints, Bc6Indices};
use crate::bc7::{Bc7Endpoints, Bc7Indices};
use crate::plain::PlainBlock;
use crate::Error;

pub(crate) const MODELS: usize = 128;
pub(crate) const BUCKETS: usize = 9;
/// Back references reach 255 blocks back; the dictionary stands in for blocks before the first.
pub(crate) const DICTIONARY: usize = 255;
/// The models' probability scale: every model's frequencies sum to this.
pub(crate) const SCALE: u32 = 1 << 16;

pub const TABLES_BYTES: usize = MODELS * BUCKETS * 2
    + (256 + 64 + 256 + 64 + 64)
    + 512 * 2
    + DICTIONARY * ((6 + 16) + (8 + 32) + (2 + 16) + (4 + 32) + (1 + 24 + 1 + 32) + (2 + 48 + 2 + 64));

/// One static rANS model. Bucket 0 is symbol 0, bucket 1 symbol 1, and bucket `b` > 1 the
/// `2^(b-1)` symbols `2^(b-1) .. 2^b`, which share one frequency. Model `16 g + s` has `g + 2`
/// buckets, so it codes symbols below `2^(g+1)`.
#[derive(Clone, Copy, Default)]
pub(crate) struct Model {
    pub(crate) freq: [u32; BUCKETS],
    /// Cumulative frequency where each bucket begins; `SCALE` for buckets the model lacks.
    pub(crate) start: [u32; BUCKETS],
    /// `ceil(2^32 / freq)` per bucket (0 for buckets the model lacks): `x / freq` is
    /// `x * recip >> 32` for every `x` below `SCALE`, without a division per symbol.
    pub(crate) recip: [u64; BUCKETS],
}

pub struct Tables {
    pub(crate) models: Box<[Model; MODELS]>,
    pub(crate) rank_bc1: [u8; 256],
    pub(crate) rank_bc3_alpha: [u8; 64],
    pub(crate) rank_bc3_color: [u8; 256],
    pub(crate) rank_bc4: [u8; 64],
    pub(crate) rank_bc5: [u8; 64],
    pub(crate) bc7_controls: [u16; 512],
    pub(crate) dict_bc1: Vec<PlainBlock>,
    pub(crate) dict_bc3: Vec<PlainBlock>,
    pub(crate) dict_bc4: Vec<PlainBlock>,
    pub(crate) dict_bc5: Vec<PlainBlock>,
    pub(crate) dict_bc6: Vec<(Bc6Endpoints, Bc6Indices)>,
    pub(crate) dict_bc7: Vec<(Bc7Endpoints, Bc7Indices)>,
}

/// Reads little-endian fields off the front of a slice whose length was checked up front.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let (head, rest) = self.0.split_at(N);
        self.0 = rest;
        head.try_into().expect("table length checked")
    }

    fn byte(&mut self) -> u8 {
        self.take::<1>()[0]
    }

    fn word(&mut self) -> u16 {
        u16::from_le_bytes(self.take())
    }
}

impl Tables {
    /// Reads and validates the table file at `path`.
    pub fn load(path: &Path) -> Result<Tables, Error> {
        let bytes = std::fs::read(path).map_err(|e| Error::TableRead(e.to_string()))?;
        Tables::parse(&bytes)
    }

    pub fn parse(bytes: &[u8]) -> Result<Tables, Error> {
        if bytes.len() != TABLES_BYTES {
            return Err(Error::TableSize(bytes.len()));
        }
        let mut c = Cursor(bytes);
        let mut models = Box::new([Model::default(); MODELS]);
        for (index, model) in models.iter_mut().enumerate() {
            for f in &mut model.freq {
                *f = u32::from(c.word());
            }
            let buckets = index / 16 + 2;
            let mut total = 0;
            for b in 0..BUCKETS {
                if b >= buckets {
                    model.start[b] = SCALE;
                    continue;
                }
                if model.freq[b] == 0 {
                    return Err(Error::TableModel(index));
                }
                model.start[b] = total;
                model.recip[b] = (1u64 << 32).div_ceil(u64::from(model.freq[b]));
                total += model.freq[b] << b.saturating_sub(1);
            }
            if total != SCALE {
                return Err(Error::TableModel(index));
            }
        }
        let rank_bc1 = c.take();
        let rank_bc3_alpha = c.take();
        let rank_bc3_color = c.take();
        let rank_bc4 = c.take();
        let rank_bc5 = c.take();
        let mut bc7_controls = [0u16; 512];
        bc7_controls.iter_mut().for_each(|w| *w = c.word());

        let mut plain = |endpoints: usize, indices: usize| -> Vec<PlainBlock> {
            (0..DICTIONARY)
                .map(|_| {
                    let mut e = PlainBlock::default();
                    e.endpoints[..endpoints].iter_mut().for_each(|v| *v = c.byte());
                    e.indices[..indices].iter_mut().for_each(|v| *v = c.byte());
                    e
                })
                .collect()
        };
        let dict_bc1 = plain(6, 16);
        let dict_bc3 = plain(8, 32);
        let dict_bc4 = plain(2, 16);
        let dict_bc5 = plain(4, 32);

        let mut dict_bc6 = Vec::with_capacity(DICTIONARY);
        for _ in 0..DICTIONARY {
            c.byte();
            let mut ends = Bc6Endpoints::default();
            ends.values.iter_mut().flatten().for_each(|v| *v = c.word());
            let mut ix = Bc6Indices { mode: c.byte(), ..Default::default() };
            ix.values.iter_mut().for_each(|v| *v = c.word() as i16);
            dict_bc6.push((ends, ix));
        }
        let mut dict_bc7 = Vec::with_capacity(DICTIONARY);
        for _ in 0..DICTIONARY {
            let mut ends = Bc7Endpoints { control: c.word(), ..Default::default() };
            ends.rows.iter_mut().flatten().for_each(|v| *v = c.word());
            let mut ix = Bc7Indices { bits: [c.byte(), c.byte()], ..Default::default() };
            ix.values.iter_mut().flatten().for_each(|v| *v = c.word() as i16);
            dict_bc7.push((ends, ix));
        }
        debug_assert!(c.0.is_empty());
        Ok(Tables {
            models,
            rank_bc1,
            rank_bc3_alpha,
            rank_bc3_color,
            rank_bc4,
            rank_bc5,
            bc7_controls,
            dict_bc1,
            dict_bc3,
            dict_bc4,
            dict_bc5,
            dict_bc6,
            dict_bc7,
        })
    }
}
