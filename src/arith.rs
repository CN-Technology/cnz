//! Binary arithmetic coder (LZMA's: 32-bit range, 12-bit probabilities,
//! byte-wise renormalisation), shared by the JPEG photo and PNG image models.
//!
//! Probabilities learn fast at first (large adaptation step), then settle.
//! The decoder does not branch on the bit value (unpredictable by nature):
//! both outcomes are computed and chosen by mask.

const PROB_BITS: u32 = 12;
const TOP: u32 = 1 << 24;

/// Adaptive probability (of a 0 bit, out of 4096). It learns fast at first
/// (large adaptation step), then settles.
#[derive(Clone, Copy, Debug)]
pub struct Prob {
    p: u16,
    n: u8,
}

/// Adaptation step according to the number of bits already seen.
const RATE: [u8; 12] = [1, 2, 2, 3, 3, 3, 4, 4, 4, 4, 5, 5];
/// Next counter (capped at 11).
const NEXT: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 11];

impl Prob {
    /// Probability of a 0, between 0 and 1 (for measurements).
    pub fn probability(self) -> f64 {
        f64::from(self.p) / f64::from(1u32 << PROB_BITS)
    }

    pub const NEW: Prob = Prob { p: 1 << (PROB_BITS - 1), n: 0 };

    #[inline(always)]
    pub fn update(&mut self, bit: bool) {
        let n = self.n as usize;
        let (p, s) = (self.p, RATE[n]);
        // Unsigned, unbounded: towards 0, p − (p >> s) stays ≥ 1; towards 4096,
        // p + ((4096 − p) >> s) stays ≤ 4095. The coder's range therefore never
        // collapses.
        self.p = if bit { p - (p >> s) } else { p + (((1 << PROB_BITS) - p) >> s) };
        self.n = NEXT[n];
    }

}

pub struct Encoder {
    out: Vec<u8>,
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
}

impl Encoder {
    pub fn new() -> Self {
        Self { out: Vec::new(), low: 0, range: u32::MAX, cache: 0, cache_size: 1 }
    }

    fn shift_low(&mut self) {
        if (self.low as u32) < 0xFF00_0000 || (self.low >> 32) != 0 {
            let carry = (self.low >> 32) as u8;
            let mut temp = self.cache;
            loop {
                self.out.push(temp.wrapping_add(carry));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = ((self.low >> 24) & 0xFF) as u8;
        }
        self.cache_size += 1;
        self.low = (self.low & 0x00FF_FFFF) << 8;
    }

    fn bit(&mut self, p: &mut Prob, bit: bool) {
        let bound = (self.range >> PROB_BITS) * u32::from(p.p);
        if bit {
            self.low += u64::from(bound);
            self.range -= bound;
        } else {
            self.range = bound;
        }
        p.update(bit);
        while self.range < TOP {
            self.range <<= 8;
            self.shift_low();
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out
    }
}

pub struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    range: u32,
    code: u32,
}

impl<'a> Decoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut d = Self { data, pos: 0, range: u32::MAX, code: 0 };
        for _ in 0..5 {
            d.code = (d.code << 8) | u32::from(d.next());
        }
        d
    }

    fn next(&mut self) -> u8 {
        // Beyond the end: zeros (decoding stays bounded by the model).
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    /// No branch on the bit value (unpredictable by nature): both outcomes are
    /// computed and chosen by mask.
    #[inline(always)]
    fn bit(&mut self, p: &mut Prob) -> bool {
        let bound = (self.range >> PROB_BITS) * u32::from(p.p);
        let bit = self.code >= bound;
        let mask = 0u32.wrapping_sub(u32::from(bit));
        self.code -= bound & mask;
        self.range = bound ^ ((bound ^ self.range.wrapping_sub(bound)) & mask);
        p.update(bit);
        while self.range < TOP {
            self.range <<= 8;
            self.code = (self.code << 8) | u32::from(self.next());
        }
        bit
    }

    /// Bytes read beyond the end of the data (must stay under 5).
    pub fn overrun(&self) -> usize {
        self.pos.saturating_sub(self.data.len())
    }
}

/// Coder or decoder, behind the same interface: the model is written once
/// and works in both directions.
pub trait Coder {
    fn code(&mut self, p: &mut Prob, bit: bool) -> bool;
    /// Category of the following bits (for measurements only).
    fn tag(&mut self, _category: usize) {}
}

impl Coder for Encoder {
    fn code(&mut self, p: &mut Prob, bit: bool) -> bool {
        self.bit(p, bit);
        bit
    }
}

impl Coder for Decoder<'_> {
    fn code(&mut self, p: &mut Prob, _: bool) -> bool {
        self.bit(p)
    }
}
