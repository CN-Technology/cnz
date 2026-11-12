//! Recompression of JPEG coefficients (stage 5, step 2).
//!
//! The coefficients decoded by jpeg.rs are re-coded with a **binary
//! arithmetic coder** (of the LZMA kind) and a model that predicts each
//! value from the neighbouring blocks already coded (above, left):
//!
//! 1. the number of non-zero AC coefficients of the block, from that of the
//!    neighbours;
//! 2. each AC coefficient, in zigzag order, as long as non-zero ones remain:
//!    zero or not, then size (number of bits), sign and bits; the context
//!    combines the position, the number of remaining non-zeros and the
//!    amplitude of the same coefficient in the neighbours;
//! 3. the DC coefficient: difference from a prediction made with the
//!    neighbouring DCs (left, above, diagonal).
//!
//! The decoder repeats exactly the same predictions.

use anyhow::{ensure, Result};

use crate::jpeg::Jpeg;

use crate::arith::{Coder, Decoder, Encoder, Prob};

/// Bit categories, to know where the space goes.
pub const CATEGORIES: [&str; 7] = ["non nuls", "AC nul ?", "AC taille", "AC signe", "AC bits", "DC", "autres"];

/// Measures the cost of each category, without writing anything.
pub struct Meter {
    pub bits: [f64; 7],
    /// Number of binary decisions of each category.
    pub decisions: [u64; 7],
    cur: usize,
}

impl Coder for Meter {
    fn code(&mut self, p: &mut Prob, bit: bool) -> bool {
        let q = p.probability();
        self.bits[self.cur] -= if bit { (1.0 - q).log2() } else { q.log2() };
        self.decisions[self.cur] += 1;
        p.update(bit);
        bit
    }
    fn tag(&mut self, category: usize) {
        self.cur = category;
    }
}

/// Estimated cost of each category, in bytes.
pub fn measure(j: &Jpeg) -> Result<[f64; 7]> {
    Ok(meter(j)?.bits.map(|b| b / 8.0))
}

/// Full measurement: cost and number of decisions per category.
pub fn meter(j: &Jpeg) -> Result<Meter> {
    let mut m = Meter { bits: [0.0; 7], decisions: [0; 7], cur: 6 };
    let mut blocks = j.blocks.clone();
    run(&mut m, j, &mut blocks)?;
    Ok(m)
}

// ---------------------------------------------------------------- model

/// Size in bits of a positive value (0 for 0).
fn bitlen(v: u32) -> u32 {
    32 - v.leading_zeros()
}

/// Groups of zigzag positions (low frequencies in detail).
const ZZ_GROUPS: usize = 12;
fn zz_group(k: usize) -> usize {
    [0, 1, 2, 3, 4, 5, 5, 6, 6, 6, 7, 7, 7, 7, 7, 8, 8, 8, 8, 8, 8, 9, 9, 9, 9, 9, 9, 9, 10, 10, 10, 10, 10, 10, 10, 10]
        .get(k)
        .copied()
        .unwrap_or(11)
}

const MAG_CTX: usize = 12;

/// Basis of the inverse transform, in integers (× 4096): C(u)·cos((2x+1)uπ/16),
/// C(0) = 1/√2. Hard-coded so that the computation is identical everywhere.
const BASIS: [[i64; 8]; 8] = [
    [2896, 2896, 2896, 2896, 2896, 2896, 2896, 2896],
    [4017, 3406, 2276, 799, -799, -2276, -3406, -4017],
    [3784, 1567, -1567, -3784, -3784, -1567, 1567, 3784],
    [3406, -799, -4017, -2276, 2276, 4017, 799, -3406],
    [2896, -2896, -2896, 2896, 2896, -2896, -2896, 2896],
    [2276, -4017, 799, 3406, -3406, -799, 4017, -2276],
    [1567, -3784, 3784, -1567, -1567, 3784, -3784, 1567],
    [799, -2276, 3406, -4017, 4017, -3406, 2276, -799],
];
/// Integer division rounded to the nearest.
fn div_round(n: i64, d: i64) -> i64 {
    if (n >= 0) == (d > 0) {
        (n + d / 2) / d
    } else {
        (n - d / 2) / d
    }
}

/// Inverse of the zigzag: reading rank of the coefficient at each position.
const UNZIG: [usize; 64] = {
    let mut u = [0; 64];
    let mut k = 0;
    while k < 64 {
        u[crate::jpeg::ZIGZAG[k]] = k;
        k += 1;
    }
    u
};

/// Number of coefficients of the 7 × 7 interior.
const INTERIOR: usize = 49;

/// Coding order of the ACs (zigzag ranks): the 7 × 7 interior in zigzag
/// order, then the first row (u = 1 to 7), then the first column.
const ORDER: [usize; 63] = {
    let mut o = [0; 63];
    let (mut n, mut k) = (0, 1);
    while k < 64 {
        let z = crate::jpeg::ZIGZAG[k];
        if z % 8 > 0 && z >= 8 {
            o[n] = k;
            n += 1;
        }
        k += 1;
    }
    let mut i = 1;
    while i < 8 {
        o[n] = UNZIG[i];
        n += 1;
        i += 1;
    }
    i = 1;
    while i < 8 {
        o[n] = UNZIG[i * 8];
        n += 1;
        i += 1;
    }
    o
};
const NZ_CTX: usize = 8;
const EXP_BITS: usize = 11;

/// Probabilities of a component class (luminance or chrominance).
struct Model {
    /// Number of non-zeros: 6-bit binary tree, per neighbourhood context.
    nz: Vec<[Prob; 64]>,
    /// AC: zero or not, per (position, remaining non-zeros, neighbouring amplitude).
    zero: Vec<Prob>,
    /// AC: size (unary), per (position, neighbouring amplitude).
    exp: Vec<[Prob; EXP_BITS]>,
    /// AC: first bit below the most significant bit, per (position, size).
    mant: Vec<Prob>,
    /// AC: sign, per (group of positions, neighbouring signs).
    sign: Vec<Prob>,
    /// DC: zero or not, size, sign, first bit, per uncertainty context.
    dc_zero: Vec<Prob>,
    dc_exp: Vec<[Prob; 12]>,
    dc_sign: Vec<Prob>,
    dc_mant: Vec<Prob>,
    /// Signs of the first row and the first column, predicted by matching the
    /// edges: per (rank, predicted size, predicted sign, size).
    edge_sign: Vec<Prob>,
    /// Remaining bits (hardly predictable).
    raw: Prob,
}

impl Model {
    fn new() -> Self {
        let p = Prob::NEW;
        Self {
            nz: vec![[p; 64]; 64],
            zero: vec![p; 64 * NZ_CTX * MAG_CTX * 3],
            exp: vec![[p; EXP_BITS]; 64 * MAG_CTX * 3],
            mant: vec![p; ZZ_GROUPS * EXP_BITS * EXP_BITS],
            sign: vec![p; ZZ_GROUPS * 9],
            dc_zero: vec![p; 12],
            dc_exp: vec![[p; 12]; 12],
            dc_sign: vec![p; 3],
            dc_mant: vec![p; 12 * 144],
            edge_sign: vec![p; 14 * 12 * 3 * 8],
            raw: p,
        }
    }
}

/// Position of each block in the grid of its component, in the file's
/// reading order: (component, x, y), and size of the grids.
fn geometry(j: &Jpeg) -> (Vec<(usize, usize, usize)>, Vec<(usize, usize)>) {
    let hmax = j.components.iter().map(|c| c.h).max().unwrap_or(1) as usize;
    let vmax = j.components.iter().map(|c| c.v).max().unwrap_or(1) as usize;
    if j.components.len() == 1 {
        let c = &j.components[0];
        let w = (j.width as usize * c.h as usize).div_ceil(hmax).div_ceil(8);
        let h = (j.height as usize * c.v as usize).div_ceil(vmax).div_ceil(8);
        let pos = (0..w * h).map(|i| (0, i % w, i / w)).collect();
        return (pos, vec![(w, h)]);
    }
    let mx = (j.width as usize).div_ceil(8 * hmax);
    let my = (j.height as usize).div_ceil(8 * vmax);
    let grids = j.components.iter().map(|c| (mx * c.h as usize, my * c.v as usize)).collect();
    let mut pos = Vec::new();
    for y in 0..my {
        for x in 0..mx {
            for (ci, c) in j.components.iter().enumerate() {
                for by in 0..c.v as usize {
                    for bx in 0..c.h as usize {
                        pos.push((ci, x * c.h as usize + bx, y * c.v as usize + by));
                    }
                }
            }
        }
    }
    (pos, grids)
}

/// The traversal shared by coding and decoding: component by component,
/// block by block in image order. `blocks` is read (coding) or filled
/// (decoding).
fn run<C: Coder>(c: &mut C, j: &Jpeg, blocks: &mut [[i16; 64]]) -> Result<()> {
    let (pos, grids) = geometry(j);
    ensure!(pos.len() == blocks.len(), "nombre de blocs incohérent");
    // Grid → block number in file order.
    let mut index: Vec<Vec<u32>> = grids.iter().map(|&(w, h)| vec![u32::MAX; w * h]).collect();
    for (n, &(ci, x, y)) in pos.iter().enumerate() {
        index[ci][y * grids[ci].0 + x] = n as u32;
    }
    let mut models = [Model::new(), Model::new()];
    for (ci, &(w, h)) in grids.iter().enumerate() {
        let m = &mut models[usize::from(ci > 0)];
        // Without a known quantisation table: 1 everywhere (the computation stays exact).
        let q = j.quant[j.components[ci].tq as usize].unwrap_or([1; 64]);
        let mut nzs = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let cur = index[ci][y * w + x] as usize;
                let above = (y > 0).then(|| index[ci][(y - 1) * w + x] as usize);
                let left = (x > 0).then(|| index[ci][y * w + x - 1] as usize);
                let diag = (x > 0 && y > 0).then(|| index[ci][(y - 1) * w + x - 1] as usize);
                code_block(c, m, &q, blocks, cur, above, left, diag, &mut nzs, y * w + x, w)?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn code_block<C: Coder>(
    c: &mut C,
    m: &mut Model,
    q: &[u16; 64],
    blocks: &mut [[i16; 64]],
    cur: usize,
    above: Option<usize>,
    left: Option<usize>,
    diag: Option<usize>,
    nzs: &mut [u8],
    cell: usize,
    w: usize,
) -> Result<()> {
    // Neighbours read in place (no copy); only the current block is copied.
    static ZERO: [i16; 64] = [0; 64];
    let mut blk = blocks[cur];
    let blocks_ro: &[[i16; 64]] = blocks;
    let a = above.map_or(&ZERO, |i| &blocks_ro[i]);
    let l = left.map_or(&ZERO, |i| &blocks_ro[i]);
    let d = diag.map_or(&ZERO, |i| &blocks_ro[i]);

    // 1. Number of non-zeros (AC), from the neighbours.
    let na = if cell >= w { u32::from(nzs[cell - w]) } else { 0 };
    let nl = if cell % w > 0 { u32::from(nzs[cell - 1]) } else { 0 };
    let n_neigh = match (cell >= w, cell % w > 0) {
        (true, true) => (na + nl).div_ceil(2),
        (true, false) => na,
        (false, true) => nl,
        (false, false) => 0,
    };
    let _ = n_neigh;
    // Context: size of each neighbour (7 × 7), or edge of the image.
    let nctx = match (cell >= w, cell % w > 0) {
        (true, true) => (bitlen(na) as usize).min(6) * 7 + (bitlen(nl) as usize).min(6),
        (true, false) => 49 + (bitlen(na) as usize).min(6),
        (false, true) => 56 + (bitlen(nl) as usize).min(6),
        (false, false) => 63,
    };
    let nz_real = blk[1..].iter().filter(|&&v| v != 0).count() as u32;
    c.tag(0);
    let mut node = 1usize;
    for i in (0..6).rev() {
        let bit = c.code(&mut m.nz[nctx][node], (nz_real >> i) & 1 == 1);
        node = node * 2 + usize::from(bit);
    }
    let nz = (node - 64) as u32;
    ensure!(nz <= 63, "nombre de coefficients invalide");
    nzs[cell] = nz as u8;

    // 2. AC coefficients, in zigzag order. The signs of the first row and the
    // first column are postponed: they will be predicted by matching the edges
    // with the neighbours, once the interior is known.
    let mut left_to_code = nz;
    // Positions (in the 8 × 8 grid) of the coefficients already known to be
    // non-zero, and zigzag ranks of the postponed signs.
    let mut known_mask = 0u64;
    let mut pending = 0u64;
    for k in 1..64 {
        if left_to_code == 0 {
            // The rest is zero (when decoding, the block is already filled with zeros).
            break;
        }
        let z = crate::jpeg::ZIGZAG[k];
        let (u, v) = (z % 8, z / 8);
        let g = zz_group(k);
        let nzc = (bitlen(left_to_code) as usize).min(NZ_CTX - 1);
        // More positions than remaining non-zeros: necessarily non-zero.
        let forced = (64 - k) as u32 == left_to_code;
        // Expected amplitude from the neighbours (Lepton weighting: above and left
        // 13, diagonal 6, out of 32).
        let (ua, ul, ud) = (i32::from(a[k]).unsigned_abs(), i32::from(l[k]).unsigned_abs(), i32::from(d[k]).unsigned_abs());
        let neigh = match (above.is_some(), left.is_some()) {
            (true, true) => (13 * ua + 13 * ul + 6 * ud + 16) / 32,
            (true, false) => ua,
            (false, true) => ul,
            (false, false) => 0,
        };
        let mag = (bitlen(neigh) as usize).min(MAG_CTX - 1);
        // Neighbours within the block (left and above in the 8 × 8 grid): on the
        // previous diagonal, so already known — except the DC (position 0), coded
        // after the ACs.
        let inside = (if u > 0 { (known_mask >> (z - 1)) & 1 } else { 0 } + if v > 0 { (known_mask >> (z - 8)) & 1 } else { 0 }) as usize;
        c.tag(1);
        let nonzero = forced || c.code(&mut m.zero[((k * NZ_CTX + nzc) * MAG_CTX + mag) * 3 + inside], blk[k] != 0);
        if !nonzero {
            blk[k] = 0;
            continue;
        }
        let deferred = (v == 0 && above.is_some()) || (u == 0 && left.is_some());
        let sa = i32::from(a[k]).signum() + 1;
        let sl = i32::from(l[k]).signum() + 1;
        let sctx = g * 9 + (sa * 3 + sl) as usize;
        let e_ctx = (k * MAG_CTX + mag) * 3 + nzc.min(4) / 2;
        let mant = &mut m.mant[g * EXP_BITS * EXP_BITS..(g + 1) * EXP_BITS * EXP_BITS];
        c.tag(2);
        let sign = if deferred { None } else { Some(&mut m.sign[sctx]) };
        let v = code_value(c, &mut m.exp[e_ctx], mant, &mut m.raw, sign, i32::from(blk[k]));
        ensure!(v != 0 && v.unsigned_abs() < 1024, "coefficient invalide");
        // Postponed sign: the absolute value is kept for now.
        blk[k] = v as i16;
        known_mask |= 1 << z;
        pending |= u64::from(deferred) << k;
        left_to_code -= 1;
    }
    ensure!(left_to_code == 0, "coefficients incohérents");
    // Postponed signs: the frequency-u (or v) component of the edge must equal
    // that of the neighbouring edge; only this coefficient is missing from the
    // sum.
    let deq = |b: &[i16; 64], n: usize| i64::from(b[UNZIG[n]]) * i64::from(q[UNZIG[n]]);
    for (e, &k) in ORDER[INTERIOR..].iter().enumerate() {
        if pending >> k & 1 == 0 {
            continue;
        }
        let z = crate::jpeg::ZIGZAG[k];
        let (u, v) = (z % 8, z / 8);
        let p = if v == 0 {
            let theirs: i64 = (0..8).map(|y| BASIS[y][7] * deq(&a, y * 8 + u)).sum();
            let mine: i64 = (1..8).map(|y| BASIS[y][0] * deq(&blk, y * 8 + u)).sum();
            div_round(theirs - mine, BASIS[0][0] * i64::from(q[k].max(1)))
        } else {
            let theirs: i64 = (0..8).map(|x| BASIS[x][7] * deq(&l, v * 8 + x)).sum();
            let mine: i64 = (1..8).map(|x| BASIS[x][0] * deq(&blk, v * 8 + x)).sum();
            div_round(theirs - mine, BASIS[0][0] * i64::from(q[k].max(1)))
        };
        let mv = blk[k].unsigned_abs();
        let pm = (bitlen(p.unsigned_abs().min(1 << 20) as u32) as usize).min(11);
        let mb = (bitlen(u32::from(mv)) as usize).min(7);
        let ctx = ((e * 12 + pm) * 3 + (p.signum() + 1) as usize) * 8 + mb;
        c.tag(3);
        let neg = c.code(&mut m.edge_sign[ctx], real_sign_negative(&blocks[cur], k));
        if neg {
            blk[k] = -(mv as i16);
        }
    }


    // 3. DC: predicted by "matching" the block's edge with those of the
    // neighbours (above, left), like Lepton. The mean pixel difference along an
    // edge depends only on the frequency-0 component of that edge: above,
    // A0 = Σ B[y][7]·F_above(0, y) must equal Σ B[y][0]·F(0, y), where only
    // F(0, 0), the DC, is unknown (likewise on the left with the rows).
    let unit = BASIS[0][0] * i64::from(q[0].max(1));
    let from_above = above.map(|_| {
        let theirs: i64 = (0..8).map(|y| BASIS[y][7] * deq(&a, y * 8)).sum();
        let mine: i64 = (1..8).map(|y| BASIS[y][0] * deq(&blk, y * 8)).sum();
        theirs - mine
    });
    let from_left = left.map(|_| {
        let theirs: i64 = (0..8).map(|x| BASIS[x][7] * deq(&l, x)).sum();
        let mine: i64 = (1..8).map(|x| BASIS[x][0] * deq(&blk, x)).sum();
        theirs - mine
    });
    let _ = &d;
    let (pred, spread) = match (from_above, from_left) {
        (Some(x), Some(y)) => {
            let pred = div_round(x + y, 2 * unit);
            (pred, (div_round(x, unit) - div_round(y, unit)).unsigned_abs())
        }
        (Some(x), None) | (None, Some(x)) => (div_round(x, unit), 0),
        (None, None) => (0, 0),
    };
    let pred = pred.clamp(-2047, 2047) as i32;
    let spread = spread.min(u64::from(u32::MAX)) as u32;
    let dctx = (bitlen(spread) as usize).min(11);
    let real = i32::from(blk[0]) - pred;
    c.tag(5);
    let nonzero = c.code(&mut m.dc_zero[dctx], real != 0);
    let r = if nonzero {
        let sign_ctx = &mut m.dc_sign[(pred.signum() + 1) as usize];
        let mant = &mut m.dc_mant[dctx * 144..(dctx + 1) * 144];
        code_value(c, &mut m.dc_exp[dctx], mant, &mut m.raw, Some(sign_ctx), real)
    } else {
        0
    };
    let dc = pred + r;
    ensure!((-2048..2048).contains(&dc), "coefficient DC invalide");
    blk[0] = dc as i16;
    blocks[cur] = blk;
    Ok(())
}

/// Actual sign of coefficient `k` (when coding; ignored when decoding).
fn real_sign_negative(blk: &[i16; 64], k: usize) -> bool {
    blk[k] < 0
}

/// Size, sign and bits of a non-zero value `v` (when coding; when decoding,
/// `v` is ignored and the value read is returned). The size is read first:
/// the context of the next first bit depends on it.
fn code_value<C: Coder>(c: &mut C, exp: &mut [Prob], mant: &mut [Prob], raw: &mut Prob, sign: Option<&mut Prob>, v: i32) -> i32 {
    // Width of the bit table (11 for ACs, 12 for the DC): that of the size
    // table.
    let n = exp.len();
    let m = v.unsigned_abs();
    let e_real = bitlen(m);
    let max_e = exp.len() as u32 + 1;
    let mut e = 1;
    while e < max_e && c.code(&mut exp[e as usize - 1], e_real > e) {
        e += 1;
    }
    let dc = exp.len() == 12;
    c.tag(if dc { 5 } else { 3 });
    // No sign: it is coded later (absolute value returned).
    let neg = match sign {
        Some(p) => c.code(p, v < 0),
        None => false,
    };
    c.tag(if dc { 5 } else { 4 });
    // Bits below the most significant bit: one context per (size, bit rank) for
    // the first two; the following ones, almost random, share one.
    let mut out = 1u32;
    for i in (0..e - 1).rev() {
        let p = if e - 1 - i <= 2 { &mut mant[((e as usize - 1) * n + i as usize).min(mant.len() - 1)] } else { &mut *raw };
        out = (out << 1) | u32::from(c.code(p, (m >> i) & 1 == 1));
    }
    if neg {
        -(out as i32)
    } else {
        out as i32
    }
}

/// Codes the coefficients of a photo.
pub fn encode(j: &Jpeg) -> Result<Vec<u8>> {
    let mut e = Encoder::new();
    let mut blocks = j.blocks.clone();
    run(&mut e, j, &mut blocks)?;
    ensure!(blocks == j.blocks, "le modèle n'a pas retrouvé les coefficients");
    Ok(e.finish())
}

/// Decodes the coefficients: `j` holds everything except the blocks (filled here).
pub fn decode(j: &mut Jpeg, data: &[u8], max_blocks: u64) -> Result<()> {
    let (pos, _) = geometry(j);
    ensure!(pos.len() as u64 <= max_blocks, "photo trop grande pour ses données");
    let mut blocks = vec![[0i16; 64]; pos.len()];
    let mut d = Decoder::new(data);
    run(&mut d, j, &mut blocks)?;
    ensure!(d.overrun() <= 5, "données de coefficients tronquées");
    j.blocks = blocks;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jpeg::{Component, Jpeg};

    fn image(w: u16, h: u16, n: usize) -> Jpeg {
        let comps = (0..n).map(|i| Component { id: i as u8 + 1, h: 1, v: 1, tq: 0, td: 0, ta: 0 }).collect();
        let mut j = Jpeg {
            header: vec![],
            width: w,
            height: h,
            components: comps,
            dc_tables: Default::default(),
            ac_tables: Default::default(),
            quant: Default::default(),
            restart: 0,
            blocks: vec![],
            pads: vec![],
            extra_zrl: vec![],
            trailer: vec![],
            progressive: false,
            scans: vec![],
            first_scan: None,
        };
        let nb = geometry(&j).0.len();
        let mut x = 12345u64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        j.blocks = (0..nb)
            .map(|_| {
                let mut b = [0i16; 64];
                b[0] = (rnd() % 400) as i16 - 200;
                for k in 1..64 {
                    if rnd() % (k as u64 + 2) == 0 {
                        b[k] = (rnd() % 60) as i16 - 30;
                    }
                }
                b
            })
            .collect();
        j
    }

    #[test]
    fn aller_retour_des_coefficients() {
        for n in [1, 3] {
            let j = image(64, 40, n);
            let data = encode(&j).unwrap();
            let mut k = Jpeg { blocks: vec![], ..j.clone() };
            decode(&mut k, &data, u64::MAX).unwrap();
            let first = k.blocks.iter().zip(&j.blocks).position(|(a, b)| a != b);
            assert_eq!(first, None, "premier bloc faux : {first:?}");
        }
    }
}
