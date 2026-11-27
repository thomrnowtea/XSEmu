//! The rANS lanes and the schedule that feeds them.
//!
//! A lane reads fragments: a header byte, a length (one byte, or two for long fragments),
//! then that many payload bytes of rANS-coded symbols. The header picks the model: its low
//! nibble is the model shape, bits 6–7 shrink the lane's alphabet, bit 4 clears the coder
//! state when the fragment ends, and bit 5 marks a long length. The coder keeps a 32-bit
//! state: it takes a byte while the state is below 2^24 and decodes a symbol while it is
//! above.
//!
//! Each round of the schedule steps every lane in order and lets the lanes that ask for
//! input take one byte each, at most 8 per round. A lane whose symbol for the current block
//! was already delivered holds the next one until every lane has caught up.

use crate::tables::{Model, Tables, SCALE};
use crate::{Error, Format};

const STATE_FLOOR: u32 = 1 << 24;
const BYTES_PER_ROUND: usize = 8;
/// Rounds in a row with no byte taken and no symbol delivered before the stream counts as stuck.
const STUCK_ROUNDS: u32 = 4;

impl Model {
    /// Decodes one symbol from `state`, returning it and the next state.
    pub(crate) fn decode(&self, state: u32) -> (u8, u32) {
        let slot = state & (SCALE - 1);
        // The starts rise, so the buckets that begin at or before the slot are a prefix:
        // counted without a branch per bucket.
        let bucket = self.start[1..].iter().map(|&s| usize::from(s <= slot)).sum::<usize>();
        let (symbol, freq, cumulative) = match bucket {
            // Slot 0 belongs to no symbol: symbol 0 owns slots 1 .. freq[0].
            0 => (0, self.freq[0].wrapping_sub(1), 1),
            b => {
                let freq = self.freq[b];
                // (slot - start) / freq, exact: the dividend is below 2^16 and freq at most 2^16
                let offset = ((u64::from(slot - self.start[b]) * self.recip[b]) >> 32) as u32;
                ((1 << (b - 1)) + offset, freq, self.start[b] + offset * freq)
            }
        };
        let next = (state >> 16).wrapping_mul(freq).wrapping_sub(cumulative).wrapping_add(slot);
        (symbol as u8, next)
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Header,
    Length,
    LengthHigh,
    Payload,
}

#[derive(Clone, Copy, Default)]
struct Lane {
    /// Symbol bits minus one at the lane's full alphabet.
    width: u8,
    phase: Phase,
    model: usize,
    long: bool,
    clear_state: bool,
    /// Payload bytes of the fragment still to read.
    remaining: u32,
    state: u32,
    wants_byte: bool,
    byte: Option<u8>,
    symbol: Option<u8>,
    /// Skips its next step: it was refused a byte or its symbol is ahead of the block.
    held: bool,
}

impl Lane {
    fn new(width: u8) -> Lane {
        Lane { width, wants_byte: true, ..Lane::default() }
    }

    /// Consumes the byte handed to it and decodes at most one symbol.
    fn step(&mut self, models: &[Model]) {
        if self.phase != Phase::Payload {
            let Some(b) = self.byte.take() else { return };
            self.wants_byte = true;
            self.read_framing(b);
            return;
        }
        let mut state = self.state;
        if state < STATE_FLOOR {
            if let Some(b) = self.byte.take() {
                self.remaining = self.remaining.wrapping_sub(1);
                state = state << 8 | u32::from(b);
            }
        }
        self.wants_byte = self.remaining != 0 && self.byte.is_none();
        if state > STATE_FLOOR {
            let (symbol, next) = models[self.model].decode(state);
            self.state = next;
            self.symbol = Some(symbol);
        } else if self.remaining != 0 {
            // At exactly 2^24 the lane neither reads nor decodes again; only a lane that
            // has delivered all its symbols may end up there.
            self.state = state;
        } else {
            // The fragment ends. A byte read in this step does not reach the kept state.
            self.phase = Phase::Header;
            self.wants_byte = self.byte.is_none();
            if self.clear_state {
                self.state = 0;
            }
        }
    }

    fn read_framing(&mut self, b: u8) {
        match self.phase {
            Phase::Header => {
                self.model = usize::from(self.width - (b >> 6)) * 16 + usize::from(b & 15);
                self.clear_state = b & 0x10 != 0;
                self.long = b & 0x20 != 0;
                self.remaining = 0;
                self.phase = Phase::Length;
            }
            Phase::Length if self.long => {
                self.remaining = u32::from(b);
                self.phase = Phase::LengthHigh;
            }
            Phase::Length => {
                self.remaining = u32::from(b) + 1;
                self.phase = Phase::Payload;
            }
            Phase::LengthHigh | Phase::Payload => {
                self.remaining = (u32::from(b) << 8 | self.remaining) + 257;
                self.phase = Phase::Payload;
            }
        }
    }
}

/// Runs the lanes of `format` until `blocks` records are complete, writing them to
/// `records` (`format.record_len()` symbols each). `next_byte(lane)` supplies the input
/// byte the schedule hands to `lane`, or `None` past the end of the stream.
pub(crate) fn decode(
    tables: &Tables,
    format: Format,
    blocks: usize,
    mut next_byte: impl FnMut(usize) -> Option<u8>,
    records: &mut [u8],
) -> Result<(), Error> {
    let widths = format.lane_widths();
    let per = format.symbols_per_lane();
    let record = format.record_len();
    let mut lanes = [Lane::default(); 16];
    let lanes = &mut lanes[..widths.len()];
    for (lane, &w) in lanes.iter_mut().zip(widths) {
        *lane = Lane::new(w);
    }
    let all_delivered = (1u32 << widths.len()) - 1;
    let mut delivered = 0u32;
    let mut stuck = 0;
    for symbol_index in 0..blocks * per {
        let at = symbol_index / per * record + symbol_index % per;
        while delivered != all_delivered {
            let mut taken = 0;
            let mut progressed = false;
            for (j, lane) in lanes.iter_mut().enumerate() {
                let bit = 1 << j;
                let refused = taken >= BYTES_PER_ROUND;
                if !lane.held {
                    lane.step(&tables.models[..]);
                }
                let ahead = lane.symbol.is_some() && delivered & bit != 0;
                if lane.wants_byte && !ahead && !refused {
                    if let Some(b) = next_byte(j) {
                        lane.byte = Some(b);
                        taken += 1;
                    }
                }
                lane.held = (lane.wants_byte && refused) || ahead;
                if !lane.held {
                    if let Some(s) = lane.symbol.take() {
                        records[at + j * per] = s;
                        delivered |= bit;
                        progressed = true;
                    }
                }
            }
            if delivered == all_delivered {
                break;
            }
            stuck = if taken > 0 || progressed { 0 } else { stuck + 1 };
            if stuck >= STUCK_ROUNDS {
                return Err(Error::Truncated);
            }
        }
        delivered = 0;
        stuck = 0;
    }
    Ok(())
}
