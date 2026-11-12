//! Context-mixing engine (stage 7, "maximum" mode).
//!
//! The file is guessed **bit by bit**. For each bit, context models give
//! their opinion:
//! - orders 0 to 6: what followed the same last bytes;
//! - the current word, and the pair "previous word + current word";
//! - the column in the line and the byte above (code, tables);
//! - "sparse" contexts and 4-byte record contexts (binary data, databases,
//!   programs);
//! - an indirect context: the bytes that followed the same pair of bytes the
//!   last time;
//! - repetitions: if the last bytes already appeared, the bit that followed
//!   them.
//!
//! Each context does not keep a probability but a compact **bit history**
//! (one byte: how many 0s and 1s recently); a table learns what each history
//! predicts. Histories are stored in groups of 15 (one nibble) in 16-byte
//! slots located by a hash, which avoids most collisions.
//!
//! Three mixers weight the opinions (in the logistic domain) according to
//! their recent reliability, three refinement tables (SSE) correct the
//! result, and the arithmetic coder writes the bit. The decoder repeats
//! exactly the same computations: everything is integer, identical on every
//! machine.
//!
//! Model no. 2 (AI mode, prototype): the same engine, plus the opinion of an
//! expert (`ia.rs`) as two mixer inputs and a final arbiter between the
//! engine and the expert. Without an expert, those inputs stay at zero and
//! model no. 1 is bit-for-bit unchanged.

use crate::ia;
use anyhow::{bail, ensure, Result};
use std::sync::Arc;

/// Contexts stored in the shared table: orders 2, 3, 4, 6, word, word pair,
/// column, sparse (2-3), order 5, order 8, records, indirect, sparse (1-3).
const N_HASHED: usize = 13;
/// Directly indexed contexts (order 0, order 1).
const N_DIRECT: usize = 2;
const N_CTX: usize = N_HASHED + N_DIRECT;
/// Mixer inputs: 2 per context, 2 for repetitions, the bias, then 2 for the
/// model no. 2 expert (its stretched opinion, its centred probability; zero
/// in model no. 1).
const N_IN: usize = 2 * N_CTX + 5;
const IA_IN: usize = 2 * N_CTX + 3;
const MATCH_BITS: u32 = 22;

/// Table size (in bits) according to the data length: a small block does
/// not need 64 MiB. The reader knows the length: same computation.
fn sizes(len: usize) -> (u32, u32) {
    let bits = usize::BITS - len.leading_zeros();
    ((bits + 5).clamp(16, TABLE_BITS), (bits + 1).clamp(12, MATCH_BITS))
}
const MIN_MATCH: usize = 7;
const MAX_MATCH: usize = 65_535;
/// Count limit of the learning tables (minimum adaptation 1/128).
const MAP_LIMIT: u16 = 127;
/// Shared table: at most 2^22 slots of 16 bytes (64 MiB), less for a small
/// block (see `sizes`).
const TABLE_BITS: u32 = 22;
/// Mixer learning (error shift).
const MIX_SHIFT: u32 = 12;
/// Refinement speed (shift).
const APM_RATE: i32 = 6;
/// Model version, at the head of the compressed data: model no. 1 is frozen
/// (archives depend on it); any change would become no. 2.
const MODEL_VERSION: u8 = 1;
/// Model no. 2: with an expert; followed by the first 8 bytes of the hash of
/// its file.
const MODEL_IA: u8 = 2;
/// Memory of one model (tables), data excluded: for the scheduler.
pub const MEMORY: u64 = 96 << 20;

/// 4096 / (1 + e^(−x/256)), x within ±2047: logistic function, in integers.
fn squash(x: i32) -> i32 {
    const T: [i32; 33] = [
        1, 2, 3, 6, 10, 16, 27, 45, 73, 120, 194, 310, 488, 747, 1101, 1546, 2047, 2549, 2994, 3348, 3607, 3785, 3901, 3975, 4024, 4050, 4068,
        4079, 4085, 4089, 4092, 4093, 4094,
    ];
    if x > 2047 {
        return 4095;
    }
    if x < -2047 {
        return 1;
    }
    let w = x & 127;
    let i = ((x >> 7) + 16) as usize;
    (T[i] * (128 - w) + T[i + 1] * w + 64) >> 7
}

/// Inverse of squash: ln(p / (1 − p)), p on 12 bits, result within ±2047.
fn stretch_table() -> Vec<i16> {
    let mut t = vec![0i16; 4096];
    let mut pi = 0usize;
    for x in -2047..=2047i32 {
        let v = squash(x) as usize;
        for p in pi..=v {
            t[p] = x as i16;
        }
        pi = v + 1;
    }
    for p in t.iter_mut().skip(pi) {
        *p = 2047;
    }
    t
}

/// Bit histories: each state stands for (n0, n1), bounded recent counts of
/// 0s and 1s; an incoming bit increases its count and reduces the other one
/// if it is large (data changes: the distant past counts less). Table built
/// at start-up, identical everywhere.
#[derive(Clone)]
struct States {
    next: Vec<[u8; 2]>,
    counts: Vec<(u8, u8)>,
}

impl States {
    fn allowed(a: u32, b: u32) -> bool {
        let (lo, hi) = (a.min(b), a.max(b));
        hi <= 40
            && lo <= match hi {
                0..=4 => hi,
                5..=7 => 4,
                8..=12 => 3,
                13..=20 => 2,
                _ => 1,
            }
    }

    fn new() -> Self {
        let mut counts: Vec<(u8, u8)> = vec![(0, 0)];
        for total in 1..=80u32 {
            for a in 0..=total {
                let b = total - a;
                if Self::allowed(a, b) {
                    counts.push((a as u8, b as u8));
                }
            }
        }
        assert!(counts.len() <= 256, "trop d'états : {}", counts.len());
        let index = |a: u32, b: u32| counts.iter().position(|&c| c == (a as u8, b as u8)).unwrap();
        let mut next = vec![[0u8; 2]; counts.len()];
        for (s, &(a, b)) in counts.iter().enumerate() {
            for bit in 0..2u32 {
                let (mut n0, mut n1) = (u32::from(a), u32::from(b));
                if bit == 0 {
                    n0 += 1;
                    if n1 > 2 {
                        n1 = n1 / 2 + 1;
                    }
                } else {
                    n1 += 1;
                    if n0 > 2 {
                        n0 = n0 / 2 + 1;
                    }
                }
                n0 = n0.min(40);
                n1 = n1.min(40);
                // Bring back into the allowed states: the opposite count is reduced.
                while !Self::allowed(n0, n1) {
                    if bit == 0 {
                        n1 -= 1;
                    } else {
                        n0 -= 1;
                    }
                }
                next[s][bit as usize] = index(n0, n1) as u8;
            }
        }
        Self { next, counts }
    }
}

/// 16-bit adaptive probability (step 1/(n + 1.5), n bounded).
#[derive(Clone, Copy)]
struct Counter {
    p: u16,
    n: u16,
}

impl Counter {
    const NEW: Counter = Counter { p: 32768, n: 0 };

    #[inline(always)]
    fn update(&mut self, bit: u32, div: &[u32], limit: u16) {
        // In unsigned 32-bit integers: (65535 − p) · step and p · step fit in 32
        // bits (step ≤ 43,690).
        let p = u32::from(self.p);
        let r = div[self.n as usize];
        let np = if bit == 1 { p + (((65535 - p) * r) >> 16) } else { p - ((p * r) >> 16) };
        self.p = np.clamp(32, 65503) as u16;
        if self.n < limit {
            self.n += 1;
        }
    }
}

/// Refinement (SSE): adjusts a probability according to a small context, by
/// interpolating between 33 points of the logistic domain.
#[derive(Clone)]
struct Apm {
    t: Vec<u16>,
    idx: usize,
    rate: i32,
}

impl Apm {
    fn new(n: usize, rate: i32) -> Self {
        let mut t = vec![0u16; n * 33];
        for i in 0..n {
            for j in 0..33 {
                t[i * 33 + j] = (squash((j as i32 - 16) * 128) * 16) as u16;
            }
        }
        Self { t, idx: 0, rate }
    }

    #[inline(always)]
    fn p(&mut self, st: i32, cx: usize) -> i32 {
        let s = st + 2048;
        let lo = (s >> 7) as usize;
        let w = s & 127;
        let base = cx * 33 + lo;
        self.idx = base + usize::from(w >= 64);
        let (a, b) = (i32::from(self.t[base]), i32::from(self.t[base + 1]));
        (a * (128 - w) + b * w) >> 11
    }

    #[inline(always)]
    fn update(&mut self, bit: u32) {
        let g = if bit == 1 { 65535 } else { 0 };
        let t = &mut self.t[self.idx];
        *t = (i32::from(*t) + ((g - i32::from(*t)) >> self.rate)) as u16;
    }
}

/// Mixer weight limit (±16, in 16-bit fixed point).
const W_MAX: i32 = (1 << 20) - 1;

/// Weighted sum: 32-bit products (weights bounded to ±2^20, opinions to
/// ±2047), 64-bit sum. Exact result, identical everywhere.
#[inline(always)]
fn dot(w: &[i32], x: &[i32; N_IN]) -> i64 {
    let mut d = 0i64;
    for i in 0..N_IN {
        d += i64::from(w[i] * x[i]);
    }
    d
}

#[inline(always)]
fn train(w: &mut [i32], x: &[i32; N_IN], err: i32, shift: u32) {
    for i in 0..N_IN {
        w[i] = (w[i] + ((x[i] * err) >> shift)).clamp(-W_MAX, W_MAX);
    }
}

/// Same computations, compiled for AVX2 (8 integers at once) when the
/// processor allows it: results are identical.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_avx2(w: &[i32], x: &[i32; N_IN]) -> i64 {
    dot(w, x)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn train_avx2(w: &mut [i32], x: &[i32; N_IN], err: i32, shift: u32) {
    train(w, x, err, shift)
}

fn has_avx2() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Mixer: weighted sum of the opinions, weight set chosen by a context.
#[derive(Clone)]
struct Mixer {
    w: Vec<i32>,
    set: usize,
    pr: i32,
    avx2: bool,
}

impl Mixer {
    fn new(sets: usize) -> Self {
        Self { w: vec![1 << 14; sets * N_IN], set: 0, pr: 2048, avx2: has_avx2() }
    }

    #[inline(always)]
    fn mix(&mut self, x: &[i32; N_IN], set: usize) -> i32 {
        self.set = set * N_IN;
        let w = &self.w[self.set..self.set + N_IN];
        #[cfg(target_arch = "x86_64")]
        let d = if self.avx2 { unsafe { dot_avx2(w, x) } } else { dot(w, x) };
        #[cfg(not(target_arch = "x86_64"))]
        let d = dot(w, x);
        let st = ((d >> 16) as i32).clamp(-2047, 2047);
        self.pr = squash(st);
        st
    }

    #[inline(always)]
    fn update(&mut self, x: &[i32; N_IN], bit: u32, shift: u32) {
        let err = ((bit as i32) << 12) - self.pr;
        let w = &mut self.w[self.set..self.set + N_IN];
        #[cfg(target_arch = "x86_64")]
        if self.avx2 {
            unsafe { train_avx2(w, x, err, shift) };
            return;
        }
        train(w, x, err, shift);
    }
}

/// Final arbiter of model no. 2: mixes the engine and the expert (stretched
/// opinions + bias), 32 weight sets according to the bit rank and the
/// engine's confidence.
#[derive(Clone)]
struct Arbitre {
    w: Vec<i32>,
    x: [i32; 3],
    set: usize,
    pr: i32,
}

impl Arbitre {
    fn new() -> Self {
        Self { w: [39_322, 26_214, 0].repeat(32), x: [0; 3], set: 0, pr: 2048 }
    }

    fn mix(&mut self, st_moteur: i32, st_ia: i32, rang: u32) -> i32 {
        self.x = [st_moteur, st_ia, 77];
        self.set = (rang as usize * 4 + (st_moteur.unsigned_abs() as usize >> 8).min(3)) * 3;
        let d: i64 = (0..3).map(|i| i64::from(self.w[self.set + i]) * i64::from(self.x[i])).sum();
        self.pr = squash(((d >> 16) as i32).clamp(-2047, 2047));
        self.pr.clamp(1, 4095)
    }

    fn update(&mut self, bit: u32) {
        let err = ((bit as i32) << 12) - self.pr;
        for i in 0..3 {
            let w = &mut self.w[self.set + i];
            *w = (*w + ((self.x[i] * err) >> 13)).clamp(-W_MAX, W_MAX);
        }
    }
}

#[derive(Clone)]
struct Model {
    /// Model no. 2: the expert being read and the final arbiter.
    ia: Option<(ia::Etat, Arbitre)>,
    ia_st: i32,
    mix3: Mixer,
    states: States,
    stretch: Vec<i16>,
    div: Vec<u32>,
    /// History table: 16-byte slots (hash + 15) of the stored contexts, then
    /// order 0 (256) and order 1 (65,536), directly indexed.
    table: Vec<u8>,
    /// Position of each context's history for the current bit.
    state_idx: [usize; N_CTX],
    tmask: usize,
    /// Slot of each stored context for the current nibble.
    bucket: [usize; N_HASHED],
    hashes: [u32; N_HASHED],
    /// Entry of the learning table used by each context.
    map_slot: [usize; N_CTX],
    maps: Vec<Counter>,
    inputs: [i32; N_IN],
    mix1: Mixer,
    mix2: Mixer,
    apm1: Apm,
    apm2: Apm,
    apm3: Apm,
    // Bytes already seen.
    buf: Vec<u8>,
    c0: u32,
    bits: u32,
    c4: u32,
    word: u32,
    prev_word: u32,
    line_start: usize,
    prev_line_start: usize,
    // Repetitions.
    /// Indirect context: the two bytes that followed each pair.
    indirect: Vec<u16>,
    mtable: Vec<u32>,
    mbits: u32,
    mptr: usize,
    mlen: usize,
    mmap: Vec<Counter>,
    mslot: usize,
    mactive: bool,
}

impl Model {
    /// `len`: data length; `capacity`: space to reserve in advance.
    fn new(len: usize, capacity: usize, expert: Option<Arc<ia::Modele>>) -> Self {
        let (tbits, mbits) = sizes(len);
        // 16-bit learning step 1/(n + 1.5): ⌊131072 / (2n + 3)⌋.
        let div = (0..=1024u32).map(|n| 131_072 / (2 * n + 3)).collect();
        let states = States::new();
        // History learning table, one per context, starting from
        // (n1 + 0.5) / (n0 + n1 + 1).
        let mut maps = Vec::with_capacity(N_CTX * 256);
        for _ in 0..N_CTX {
            for s in 0..256 {
                let (a, b) = states.counts.get(s).copied().unwrap_or((0, 0));
                let (a, b) = (u32::from(a), u32::from(b));
                // ⌊65535 · (2n1 + 1) / (2n0 + 2n1 + 2)⌋
                maps.push(Counter { p: (65_535 * (2 * b + 1) / (2 * a + 2 * b + 2)) as u16, n: 0 });
            }
        }
        Self {
            ia: expert.map(|m| (ia::Etat::new(m), Arbitre::new())),
            ia_st: 0,
            mix3: Mixer::new(256),
            states,
            stretch: stretch_table(),
            div,
            table: vec![0; (16 << tbits) + 256 + (1 << 16)],
            state_idx: [0; N_CTX],
            tmask: (1 << tbits) - 1,
            bucket: [0; N_HASHED],
            hashes: [0; N_HASHED],
            map_slot: [0; N_CTX],
            maps,
            inputs: [0; N_IN],
            mix1: Mixer::new(256),
            mix2: Mixer::new(4 * 8 * 2 * 4),
            apm1: Apm::new(256, APM_RATE),
            apm2: Apm::new(1 << 16, APM_RATE),
            apm3: Apm::new(1 << 16, APM_RATE),
            indirect: vec![0; 1 << 16],
            buf: Vec::with_capacity(capacity),
            c0: 1,
            bits: 0,
            c4: 0,
            word: 0,
            prev_word: 0,
            line_start: 0,
            prev_line_start: 0,
            mtable: vec![0; 1 << mbits],
            mbits,
            mptr: 0,
            mlen: 0,
            mmap: vec![Counter::NEW; 64],
            mslot: 0,
            mactive: false,
        }
    }

    /// 16-byte slot for hash `h`: searched among 3 neighbours (8-bit check);
    /// otherwise the least useful one is emptied.
    fn find(&mut self, h: u32) -> usize {
        let chk = (h >> 24) as u8;
        let base = (h as usize & self.tmask) << 4;
        let mut best = base;
        let mut best_n = u32::MAX;
        for k in 0..3 {
            let b = base ^ (k << 4);
            if self.table[b] == chk {
                return b;
            }
            // Usefulness: the state of the first bit (states are numbered by
            // increasing number of bits seen).
            let n = u32::from(self.table[b + 1]);
            if n < best_n {
                best_n = n;
                best = b;
            }
        }
        self.table[best..best + 16].fill(0);
        self.table[best] = chk;
        best
    }

    #[inline(always)]
    fn st(&self, p16: u16) -> i32 {
        i32::from(self.stretch[usize::from(p16 >> 4)])
    }

    /// Position in the current nibble: 1 followed by the bits already read.
    #[inline(always)]
    fn nib(&self) -> usize {
        (if self.bits < 4 { self.c0 } else { (self.c0 & ((1 << (self.bits - 4)) - 1)) | 1 << (self.bits - 4) }) as usize
    }

    /// Probability (on 12 bits) that the next bit is 1.
    fn predict(&mut self) -> i32 {
        // At the start of each nibble: slots of the stored contexts.
        if self.bits == 0 || self.bits == 4 {
            for i in 0..N_HASHED {
                let h = (self.hashes[i] ^ self.c0.wrapping_mul(0x9E37_79B1)).wrapping_mul(0x2C1B_3C6D);
                let h = (h ^ (h >> 15)).wrapping_mul(0x85EB_CA6B);
                self.bucket[i] = self.find(h ^ (h >> 13));
            }
        }
        let nib = self.nib();
        for i in 0..N_HASHED {
            self.state_idx[i] = self.bucket[i] + nib;
        }
        let direct = (self.tmask + 1) << 4;
        self.state_idx[N_HASHED] = direct + self.c0 as usize;
        self.state_idx[N_HASHED + 1] = direct + 256 + ((self.c4 as usize & 0xFF) << 8 | self.c0 as usize);
        for i in 0..N_CTX {
            let state = self.table[self.state_idx[i]];
            let m = i * 256 + usize::from(state);
            self.map_slot[i] = m;
            if state == 0 {
                self.inputs[2 * i] = 0;
                self.inputs[2 * i + 1] = 0;
            } else {
                let p = self.maps[m].p;
                self.inputs[2 * i] = self.st(p);
                self.inputs[2 * i + 1] = (i32::from(p >> 4) - 2048) >> 1;
            }
        }
        // Repetition: the expected bit, with a confidence learned according to the
        // length of the repetition.
        self.mactive = false;
        let mut expect = 0u32;
        if self.mlen > 0 {
            let pb = u32::from(self.buf[self.mptr]) | 256;
            if pb >> (8 - self.bits) == self.c0 {
                expect = (pb >> (7 - self.bits)) & 1;
                let lb = if self.mlen < 16 { self.mlen } else { 16 + (self.mlen.min(1023) >> 6) };
                self.mslot = lb.min(31) * 2 + expect as usize;
                self.inputs[2 * N_CTX] = self.st(self.mmap[self.mslot].p);
                let strength = self.mlen.min(32) as i32 * 32;
                self.inputs[2 * N_CTX + 1] = if expect == 1 { strength } else { -strength };
                self.mactive = true;
            } else {
                self.mlen = 0;
            }
        }
        if !self.mactive {
            self.inputs[2 * N_CTX] = 0;
            self.inputs[2 * N_CTX + 1] = 0;
        }
        self.inputs[2 * N_CTX + 2] = 256;
        if let Some((etat, _)) = &self.ia {
            let p = (i32::from(etat.proba(self.c0)) >> 4).clamp(1, 4095);
            self.ia_st = i32::from(self.stretch[p as usize]);
            self.inputs[IA_IN] = self.ia_st;
            self.inputs[IA_IN + 1] = (p - 2048) >> 1;
        }
        let mbucket = match self.mlen {
            0 => 0,
            1..=15 => 1,
            16..=31 => 2,
            _ => 3,
        };
        let prev = (self.c4 & 0xFF) as usize;
        let s1 = self.mix1.mix(&self.inputs, self.c0 as usize);
        let set2 = ((mbucket * 8 + self.bits as usize) * 2 + expect as usize) * 4 + (prev >> 6);
        let s2 = self.mix2.mix(&self.inputs, set2);
        let s3 = self.mix3.mix(&self.inputs, prev);
        let st = (s1 + s2 + s3) / 3;
        let pr_mix = squash(st);
        // Refinements: current byte; + previous byte; + two previous bytes.
        let a1 = self.apm1.p(st, self.c0 as usize);
        let a2 = self.apm2.p(st, self.c0 as usize | prev << 8);
        let h2 = ((self.c4 & 0xFFFF).wrapping_mul(0x9E37_79B1) >> 24) as usize;
        let a3 = self.apm3.p(st, self.c0 as usize | h2 << 8);
        let p = ((pr_mix + a1 + a2 * 3 + a3 * 3 + 4) >> 3).clamp(1, 4095);
        match &mut self.ia {
            Some((_, arbitre)) => arbitre.mix(i32::from(self.stretch[p as usize]), self.ia_st, self.bits),
            None => p,
        }
    }

    fn update(&mut self, bit: u32) {
        self.mix1.update(&self.inputs, bit, MIX_SHIFT);
        self.mix2.update(&self.inputs, bit, MIX_SHIFT);
        self.mix3.update(&self.inputs, bit, MIX_SHIFT);
        for i in 0..N_CTX {
            self.maps[self.map_slot[i]].update(bit, &self.div, MAP_LIMIT);
            let s = &mut self.table[self.state_idx[i]];
            *s = self.states.next[usize::from(*s)][bit as usize];
        }
        if self.mactive {
            self.mmap[self.mslot].update(bit, &self.div, MAP_LIMIT);
        }
        self.apm1.update(bit);
        self.apm2.update(bit);
        self.apm3.update(bit);
        if let Some((_, arbitre)) = &mut self.ia {
            arbitre.update(bit);
        }

        self.c0 = (self.c0 << 1) | bit;
        self.bits += 1;
        if self.bits == 8 {
            if let Some((etat, _)) = &mut self.ia {
                etat.avancer((self.c0 & 0xFF) as u8);
            }
            self.byte_done((self.c0 & 0xFF) as u8);
            self.c0 = 1;
            self.bits = 0;
        }
    }

    fn byte_done(&mut self, c: u8) {
        let pair = (self.c4 & 0xFFFF) as usize;
        self.indirect[pair] = self.indirect[pair] << 8 | u16::from(c);
        self.buf.push(c);
        self.c4 = (self.c4 << 8) | u32::from(c);
        let n = self.buf.len();
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_alphanumeric() || c >= 0x80 || c == b'_' {
            self.word = (self.word ^ u32::from(lc)).wrapping_mul(0x0100_0193);
        } else if self.word != 0 {
            self.prev_word = self.word;
            self.word = 0;
        }
        if c == b'\n' {
            self.prev_line_start = self.line_start;
            self.line_start = n;
        }
        let b = |k: usize| if n >= k { u32::from(self.buf[n - k]) } else { 0 };
        let col = n - self.line_start;
        let above = if self.prev_line_start + col < self.line_start { u32::from(self.buf[self.prev_line_start + col]) } else { 0 };
        let mix = |a: u32, k: u32| a.wrapping_add(k).wrapping_mul(0x9E37_79B1) ^ k.wrapping_mul(0x85EB_CA6B);
        self.hashes = [
            mix(self.c4 & 0xFFFF, 2),
            mix(self.c4 & 0xFF_FFFF, 3),
            mix(self.c4, 4),
            mix(self.c4 ^ (b(5) | b(6) << 8).wrapping_mul(0x2C1B_3C6D), 6),
            mix(self.word ^ (b(1) << 24), 7),
            mix(self.word.wrapping_add(self.prev_word.wrapping_mul(0x2C1B_3C6D)), 8),
            mix(above | (col.min(255) as u32) << 8 | b(1) << 16, 9),
            mix(b(2) | b(3) << 8, 10),
            mix(self.c4 ^ b(5).wrapping_mul(0x2C1B_3C6D), 5),
            mix(self.c4 ^ (b(5) | b(6) << 8 | b(7) << 16 | b(8) << 24).wrapping_mul(0x2C1B_3C6D), 11),
            mix(b(4) | b(8) << 8 | ((n & 3) as u32) << 16, 12),
            mix(u32::from(self.indirect[(self.c4 & 0xFFFF) as usize]) | b(1) << 16, 13),
            mix(b(1) | b(3) << 8, 14),
        ];
        // Repetitions: extend, otherwise look up the last MIN_MATCH bytes.
        if self.mlen > 0 {
            self.mptr += 1;
            if self.mlen < MAX_MATCH {
                self.mlen += 1;
            }
        }
        if n >= MIN_MATCH {
            let h = (self.buf[n - MIN_MATCH..].iter().fold(0u32, |h, &x| (h ^ u32::from(x)).wrapping_mul(0x0100_0193)) >> (32 - self.mbits)) as usize;
            if self.mlen == 0 {
                let cand = self.mtable[h] as usize;
                if cand > 0 {
                    let mut l = 0;
                    while l < 64 && l < cand && self.buf[cand - 1 - l] == self.buf[n - 1 - l] {
                        l += 1;
                    }
                    if l >= MIN_MATCH {
                        self.mlen = l;
                        self.mptr = cand;
                    }
                }
            }
            self.mtable[h] = n as u32;
        }
        if self.mlen > 0 && self.mptr >= n {
            self.mlen = 0;
        }
    }
}

/// 32-bit binary arithmetic coder, 12-bit probabilities.
struct Encoder {
    x1: u32,
    x2: u32,
    out: Vec<u8>,
}

impl Encoder {
    #[inline(always)]
    fn code(&mut self, bit: u32, p: i32) {
        let xmid = self.x1 + ((u64::from(self.x2 - self.x1) * p as u64) >> 12) as u32;
        if bit == 1 {
            self.x2 = xmid;
        } else {
            self.x1 = xmid + 1;
        }
        while (self.x1 ^ self.x2) & 0xFF00_0000 == 0 {
            self.out.push((self.x2 >> 24) as u8);
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
        }
    }
}

/// Compresses `data` (its size is known to the reader from elsewhere).
pub fn compress(data: &[u8]) -> Vec<u8> {
    compress_with(data, &mut |_| Ok(())).unwrap()
}

/// Like `compress`, calling `tick` every 256 KiB with the number of bytes
/// processed since the previous call (progress, stop request).
pub fn compress_with(data: &[u8], tick: &mut dyn FnMut(u64) -> Result<()>) -> Result<Vec<u8>> {
    compress_model(data, None, tick)
}

/// Model no. 2: like `compress_with`, with the opinion of `expert` (much
/// slower: see `ia.rs`).
pub fn compress_ia(data: &[u8], expert: &ia::Expert, tick: &mut dyn FnMut(u64) -> Result<()>) -> Result<Vec<u8>> {
    compress_model(data, Some(expert), tick)
}

fn compress_model(data: &[u8], expert: Option<&ia::Expert>, tick: &mut dyn FnMut(u64) -> Result<()>) -> Result<Vec<u8>> {
    let mut m = Model::new(data.len(), data.len(), expert.map(|e| e.modele.clone()));
    let mut e = Encoder { x1: 0, x2: u32::MAX, out: Vec::with_capacity(data.len() / 3 + 16) };
    match expert {
        Some(x) => {
            e.out.push(MODEL_IA);
            e.out.extend_from_slice(&x.empreinte);
        }
        None => e.out.push(MODEL_VERSION),
    }
    // Finer progress with an expert (a few KB per second).
    let tranche = if expert.is_some() { 4 << 10 } else { 256 << 10 };
    for chunk in data.chunks(tranche) {
        for &c in chunk {
            for k in (0..8).rev() {
                let bit = u32::from(c >> k) & 1;
                let p = m.predict();
                e.code(bit, p);
                m.update(bit);
            }
        }
        tick(chunk.len() as u64)?;
    }
    e.out.extend_from_slice(&e.x1.to_be_bytes());
    Ok(e.out)
}

/// For measurements (AI mode prototype): 12-bit probability that each bit of
/// `data` is 1, in the order in which the compressor codes them.
#[doc(hidden)]
pub fn probabilities(data: &[u8]) -> Vec<u16> {
    let mut m = Model::new(data.len(), data.len(), None);
    let mut out = Vec::with_capacity(data.len() * 8);
    for &c in data {
        for k in (0..8).rev() {
            let bit = u32::from(c >> k) & 1;
            out.push(m.predict() as u16);
            m.update(bit);
        }
    }
    out
}

/// For measurements (choice of warm-up data): an engine that reads without
/// writing anything, can be copied, and gives the cost in bits of what it
/// reads.
#[doc(hidden)]
#[derive(Clone)]
pub struct Banc(Model);

#[doc(hidden)]
impl Banc {
    /// Tables sized as for a block of `taille` bytes.
    pub fn new(taille: usize) -> Self {
        Banc(Model::new(taille, taille, None))
    }

    /// Reads `data` and returns its cost in bits (−log2 of the probabilities).
    pub fn lire(&mut self, data: &[u8]) -> f64 {
        let mut bits = 0.0;
        for &c in data {
            for k in (0..8).rev() {
                let bit = u32::from(c >> k) & 1;
                let p = f64::from(self.0.predict()) / 4096.0;
                bits -= if bit == 1 { p } else { 1.0 - p }.log2();
                self.0.update(bit);
            }
        }
        bits
    }
}

/// Data compressed with an expert (model no. 2)?
pub fn uses_ia(comp: &[u8]) -> bool {
    comp.first() == Some(&MODEL_IA)
}

/// Decompresses `len` bytes.
pub fn decompress(comp: &[u8], len: usize) -> Result<Vec<u8>> {
    decompress_with(comp, len, &mut || Ok(()))
}

/// Like `decompress`, calling `check` every 256 KiB (stop request).
pub fn decompress_with(comp: &[u8], len: usize, check: &mut dyn FnMut() -> Result<()>) -> Result<Vec<u8>> {
    decompress_model(comp, len, check, &|e| ia::par_empreinte(e).map(|x| x.modele.clone()))
}

/// `trouver`: the installed expert that has this hash.
fn decompress_model(
    comp: &[u8],
    len: usize,
    check: &mut dyn FnMut() -> Result<()>,
    trouver: &dyn Fn(&[u8]) -> Option<Arc<ia::Modele>>,
) -> Result<Vec<u8>> {
    let (expert, comp) = match comp.first() {
        Some(&MODEL_VERSION) => (None, &comp[1..]),
        Some(&MODEL_IA) if comp.len() >= 9 => match trouver(&comp[1..9]) {
            Some(m) => (Some(m), &comp[9..]),
            None => bail!(
                "ce bloc a été compressé avec un expert d'IA absent (empreinte {}) : installez le même fichier .cnzia dans le dossier des modèles",
                comp[1..9].iter().map(|b| format!("{b:02x}")).collect::<String>()
            ),
        },
        _ => bail!("moteur à mélange de contextes : version inconnue"),
    };
    let ia = expert.is_some();
    // Space is reserved as we go: a huge announced size for a few bytes does
    // not make everything be reserved in advance.
    let mut m = Model::new(len, len.min(64 << 20), expert);
    let (mut x1, mut x2) = (0u32, u32::MAX);
    let mut pos = 0;
    let mut next = || {
        let b = comp.get(pos).copied().unwrap_or(0);
        pos += 1;
        b
    };
    let mut x = 0u32;
    for _ in 0..4 {
        x = (x << 8) | u32::from(next());
    }
    let mut out = Vec::with_capacity(len.min(64 << 20));
    for k in 0..len {
        if k & 0x3_FFFF == 0 || ia && k & 0xFFF == 0 {
            check()?;
        }
        let mut c = 0u32;
        for _ in 0..8 {
            let p = m.predict();
            let xmid = x1 + ((u64::from(x2 - x1) * p as u64) >> 12) as u32;
            let bit = u32::from(x <= xmid);
            if bit == 1 {
                x2 = xmid;
            } else {
                x1 = xmid + 1;
            }
            m.update(bit);
            c = (c << 1) | bit;
            while (x1 ^ x2) & 0xFF00_0000 == 0 {
                x1 <<= 8;
                x2 = (x2 << 8) | 255;
                x = (x << 8) | u32::from(next());
            }
        }
        out.push(c as u8);
    }
    ensure!(pos <= comp.len() + 4, "données compressées trop courtes");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etats() {
        let s = States::new();
        assert!(s.counts.len() <= 256);
        assert!(s.next.iter().all(|t| usize::from(t[0]) < s.counts.len() && usize::from(t[1]) < s.counts.len()));
    }

    /// Model no. 1 is frozen: same data, same compressed bytes, on every
    /// machine. If this test breaks, the model has changed: a model no. 2 is
    /// needed, not a tweak of no. 1.
    #[test]
    fn modele_fige() {
        let mut x = 7u32;
        let data: Vec<u8> = (0..200_000u32)
            .map(|i| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                if i % 3000 < 1500 { b"function cnz(a, b) { return a + b; }\n"[i as usize % 37] } else { (x >> 24) as u8 & 0x0F }
            })
            .collect();
        let c = compress(&data);
        assert_eq!(c[0], MODEL_VERSION);
        assert_eq!(&blake3::hash(&c).to_hex()[..16], EMPREINTE_MODELE);
    }

    const EMPREINTE_MODELE: &str = "1529149a24277c15";

    #[test]
    fn aller_retour() {
        let texte: Vec<u8> = (0..3000).flat_map(|i| format!("ligne {i} : bonjour de CN Technology, {}\n", i * 7 % 13).into_bytes()).collect();
        let mut bruit = 0x1234_5678u32;
        let alea: Vec<u8> = (0..20_000)
            .map(|_| {
                bruit ^= bruit << 13;
                bruit ^= bruit >> 17;
                bruit ^= bruit << 5;
                bruit as u8
            })
            .collect();
        for d in [&texte[..], &alea[..], b"", b"a", &[0u8; 5000][..]] {
            let c = compress(d);
            assert_eq!(decompress(&c, d.len()).unwrap(), d);
        }
        assert!(compress(&texte).len() < zstd::bulk::compress(&texte, 19).unwrap().len());
    }

    /// Model no. 2 with a small test expert: exact round trip, and a clear
    /// refusal without the expert.
    #[test]
    fn aller_retour_ia() {
        let expert = ia::Expert { nom: "test".into(), empreinte: *b"TESTTEST", modele: Arc::new(ia::tests::petit_modele()) };
        let texte: Vec<u8> = (0..400).flat_map(|i| format!("Le moteur devine la suite, ligne {i}.\n").into_bytes()).collect();
        for d in [&texte[..], b"", b"z"] {
            let c = compress_ia(d, &expert, &mut |_| Ok(())).unwrap();
            assert!(uses_ia(&c) && c[1..9] == expert.empreinte);
            let trouver = |e: &[u8]| (e == expert.empreinte).then(|| expert.modele.clone());
            assert_eq!(decompress_model(&c, d.len(), &mut || Ok(()), &trouver).unwrap(), d);
            assert!(decompress_model(&c, d.len(), &mut || Ok(()), &|_| None).is_err());
        }
    }
}
