//! Exact JPEG reader and rewriter (stage 5, step 1).
//!
//! A "sequential" JPEG photo (the case of almost every device) contains,
//! after its headers, Huffman-coded image **coefficients**. They are
//! decoded, and the file can then be rewritten **byte for byte**: everything
//! the encoder could choose freely is recorded (fill bits, superfluous
//! "16 zeros" codes before an end of block).
//!
//! What precedes the image data (headers, EXIF, thumbnail) and what follows
//! the end of the image (data appended by some phones) is kept as is.
//! Anything outside this frame (progressive, lossless, arithmetic, 12-bit
//! JPEG…) is refused: the photo stays stored as is.
//!
//! This is the basis of the recompressor: the coefficients are then coded
//! with a model far more efficient than the original Huffman coding.

use anyhow::{bail, ensure, Context, Result};

/// Zigzag order: position in the 8×8 block of the k-th coefficient read.
pub const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47,
    55, 62, 63,
];

/// A component of the image (luminance, chrominances).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    pub id: u8,
    /// Horizontal and vertical sampling factors (1 to 4).
    pub h: u8,
    pub v: u8,
    /// Quantisation table used (for the model, later).
    pub tq: u8,
    /// Huffman tables of the scan (DC, AC).
    pub td: u8,
    pub ta: u8,
}

/// Huffman table as written in the file.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Huffman {
    /// Number of codes of each length (1 to 16 bits).
    pub bits: [u8; 16],
    pub values: Vec<u8>,
}

/// Codes of a table: for each symbol, (code, length).
pub(crate) fn build_codes(h: &Huffman) -> Result<[(u16, u8); 256]> {
    let mut codes = [(0u16, 0u8); 256];
    let mut code = 0u32;
    let mut k = 0;
    for len in 1..=16u8 {
        for _ in 0..h.bits[len as usize - 1] {
            let sym = *h.values.get(k).context("table de Huffman incomplète")?;
            ensure!(code < (1 << len), "table de Huffman invalide");
            ensure!(codes[sym as usize].1 == 0, "symbole en double dans la table");
            codes[sym as usize] = (code as u16, len);
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    Ok(codes)
}

/// Decoder: direct table for codes of at most 9 bits, and for the others,
/// largest code and position of the first symbol of each length (JPEG
/// standard, F.2.2.3).
pub(crate) struct Decoder {
    fast: Vec<(u8, u8)>,
    maxcode: [i32; 17],
    valptr: [i32; 17],
    mincode: [i32; 17],
    values: Vec<u8>,
}

const FAST_BITS: u32 = 9;

impl Decoder {
    pub(crate) fn new(h: &Huffman) -> Result<Self> {
        let codes = build_codes(h)?;
        let (mut maxcode, mut valptr, mut mincode) = ([-1i32; 17], [0i32; 17], [0i32; 17]);
        let (mut code, mut k) = (0i32, 0i32);
        for len in 1..=16 {
            let n = i32::from(h.bits[len - 1]);
            if n > 0 {
                valptr[len] = k;
                mincode[len] = code;
                code += n;
                k += n;
                maxcode[len] = code - 1;
            }
            code <<= 1;
        }
        let mut fast = vec![(0u8, 0u8); 1 << FAST_BITS];
        for (sym, &(c, len)) in codes.iter().enumerate() {
            if len > 0 && u32::from(len) <= FAST_BITS {
                let shift = FAST_BITS - u32::from(len);
                let base = (c as usize) << shift;
                for e in fast.iter_mut().skip(base).take(1 << shift) {
                    *e = (sym as u8, len);
                }
            }
        }
        Ok(Self { fast, maxcode, valptr, mincode, values: h.values.clone() })
    }

    pub(crate) fn decode(&self, r: &mut BitReader) -> Result<u8> {
        r.refill();
        let (sym, len) = self.fast[r.peek(FAST_BITS) as usize];
        if len > 0 {
            r.consume(u32::from(len))?;
            return Ok(sym);
        }
        let bits = r.peek(16) as i32;
        for len in (FAST_BITS as usize + 1)..=16 {
            let code = bits >> (16 - len);
            if code <= self.maxcode[len] {
                r.consume(len as u32)?;
                let i = (self.valptr[len] + code - self.mincode[len]) as usize;
                return self.values.get(i).copied().context("code de Huffman invalide");
            }
        }
        bail!("code de Huffman invalide")
    }
}

/// Reading the bits of a data segment already stripped of its stuffing
/// (bytes 0xFF 0x00 turned into 0xFF): 64 bits at a time.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    n: u32,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, acc: 0, n: 0 }
    }

    fn refill(&mut self) {
        while self.n <= 56 && self.pos < self.data.len() {
            self.acc |= u64::from(self.data[self.pos]) << (56 - self.n);
            self.pos += 1;
            self.n += 8;
        }
    }

    /// The next `k` bits (zeros beyond the end).
    fn peek(&self, k: u32) -> u64 {
        self.acc >> (64 - k)
    }

    fn consume(&mut self, k: u32) -> Result<()> {
        ensure!(k <= self.n, "données d'image tronquées");
        self.acc <<= k;
        self.n -= k;
        Ok(())
    }

    pub(crate) fn bits(&mut self, k: u8) -> Result<u16> {
        if k == 0 {
            return Ok(0);
        }
        self.refill();
        let v = self.peek(u32::from(k)) as u16;
        self.consume(u32::from(k))?;
        Ok(v)
    }

    /// End of the segment: only fill bits may remain (less than one byte).
    /// Returns (value, count).
    pub(crate) fn rest(&mut self) -> Result<(u8, u8)> {
        let left = self.n as usize + (self.data.len() - self.pos) * 8;
        ensure!(left < 8, "données en trop à la fin d'un segment");
        let v = if self.n == 0 { 0 } else { self.peek(self.n) as u8 };
        Ok((v, self.n as u8))
    }
}

/// Image data: segments separated by restart markers, stuffing bytes
/// removed. Also returns the position of the end-of-image marker.
pub(crate) fn unstuff(d: &[u8], start: usize) -> Result<(Vec<Vec<u8>>, usize)> {
    let mut segments = Vec::new();
    let mut cur = Vec::new();
    let mut i = start;
    loop {
        let rest = d.get(i..).context("données d'image tronquées")?;
        let Some(ff) = rest.iter().position(|&b| b == 0xFF) else { bail!("fin d'image absente") };
        cur.extend_from_slice(&rest[..ff]);
        i += ff;
        match d.get(i + 1) {
            Some(0x00) => {
                cur.push(0xFF);
                i += 2;
            }
            Some(&m @ 0xD0..=0xD7) => {
                ensure!(m - 0xD0 == (segments.len() % 8) as u8, "marqueur de reprise inattendu");
                segments.push(std::mem::take(&mut cur));
                i += 2;
            }
            // Any other marker ends the data: end of image, or (progressive JPEG)
            // tables and next scan.
            Some(0x01..=0xFE) => {
                segments.push(cur);
                return Ok((segments, i));
            }
            _ => bail!("marqueur invalide dans les données d'image"),
        }
    }
}

#[derive(Default)]
pub(crate) struct BitWriter {
    pub(crate) out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    pub(crate) fn put(&mut self, v: u16, len: u8) {
        self.acc = (self.acc << len) | u64::from(v);
        self.n += u32::from(len);
        while self.n >= 8 {
            let b = (self.acc >> (self.n - 8)) as u8;
            self.out.push(b);
            if b == 0xFF {
                self.out.push(0);
            }
            self.n -= 8;
        }
        self.acc &= (1u64 << self.n) - 1;
    }

    pub(crate) fn aligned(&self) -> bool {
        self.n == 0
    }
}

/// Value of a coefficient from its category and its bits.
pub(crate) fn extend(v: u16, s: u8) -> i16 {
    if s == 0 {
        return 0;
    }
    let v = i32::from(v);
    if v < 1 << (s - 1) {
        (v - (1 << s) + 1) as i16
    } else {
        v as i16
    }
}

/// Category and bits of a value.
pub(crate) fn category(v: i16) -> (u8, u16) {
    let a = i32::from(v).unsigned_abs();
    let s = (32 - a.leading_zeros()) as u8;
    let bits = if v < 0 { (i32::from(v) - 1) as u32 & ((1u32 << s) - 1) } else { a };
    (s, bits as u16)
}

/// A decoded photo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Jpeg {
    /// Everything up to the end of the scan header (SOS) included.
    pub header: Vec<u8>,
    pub width: u16,
    pub height: u16,
    pub components: Vec<Component>,
    pub dc_tables: [Option<Huffman>; 4],
    pub ac_tables: [Option<Huffman>; 4],
    /// Quantisation tables (for the model, later).
    pub quant: [Option<[u16; 64]>; 4],
    /// Restart interval in MCUs (0: none).
    pub restart: u16,
    /// Coefficients of each block, in reading order, in zigzag.
    pub blocks: Vec<[i16; 64]>,
    /// Fill bits at the end of each segment (value, count).
    pub pads: Vec<(u8, u8)>,
    /// Blocks with superfluous "16 zeros" codes before the end of block:
    /// (block number, number of codes, end of block written or not).
    pub extra_zrl: Vec<(u32, u8, bool)>,
    /// Everything that follows the image data (end marker included).
    pub trailer: Vec<u8>,
    /// Progressive JPEG: one scan per pass (jpegprog.rs).
    pub progressive: bool,
    pub scans: Vec<crate::jpegprog::Scan>,
    /// Parameters of the first scan (read from the headers).
    pub first_scan: Option<ScanParams>,
}

/// Parameters of a scan (SOS header): components (index in the image, DC
/// table, AC table), coefficient band and successive approximation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanParams {
    pub comps: Vec<(usize, u8, u8)>,
    pub ss: u8,
    pub se: u8,
    pub ah: u8,
    pub al: u8,
}

impl Jpeg {
    /// Number of blocks of each component in an MCU, and number of MCUs.
    fn layout(&self) -> (Vec<usize>, usize) {
        let hmax = self.components.iter().map(|c| c.h).max().unwrap_or(1) as usize;
        let vmax = self.components.iter().map(|c| c.v).max().unwrap_or(1) as usize;
        if self.components.len() == 1 {
            // Scan of a single component: one MCU = one block.
            let c = &self.components[0];
            let w = (self.width as usize * c.h as usize).div_ceil(hmax).div_ceil(8);
            let h = (self.height as usize * c.v as usize).div_ceil(vmax).div_ceil(8);
            return (vec![1], w * h);
        }
        let mx = (self.width as usize).div_ceil(8 * hmax);
        let my = (self.height as usize).div_ceil(8 * vmax);
        (self.components.iter().map(|c| c.h as usize * c.v as usize).collect(), mx * my)
    }
}

fn be16(d: &[u8], i: usize) -> Result<usize> {
    Ok(usize::from(u16::from_be_bytes(d.get(i..i + 2).context("en-tête JPEG tronqué")?.try_into().unwrap())))
}

/// Decodes a JPEG photo (sequential or progressive).
pub fn decode(d: &[u8]) -> Result<Jpeg> {
    let mut j = parse_header(d)?;
    if j.progressive {
        crate::jpegprog::decode(d, &mut j)?;
    } else {
        decode_scan(d, &mut j)?;
    }
    Ok(j)
}

/// Reads the headers up to the start of the image data (scan included);
/// `d` may contain only those headers. The blocks stay empty.
pub fn parse_header(d: &[u8]) -> Result<Jpeg> {
    ensure!(d.starts_with(&[0xFF, 0xD8]), "pas un JPEG");
    let mut j = Jpeg {
        header: Vec::new(),
        width: 0,
        height: 0,
        components: Vec::new(),
        dc_tables: Default::default(),
        ac_tables: Default::default(),
        quant: Default::default(),
        restart: 0,
        blocks: Vec::new(),
        pads: Vec::new(),
        extra_zrl: Vec::new(),
        trailer: Vec::new(),
        progressive: false,
        scans: Vec::new(),
        first_scan: None,
    };
    let mut frame = false;
    let (scan_start, params) = read_segments(d, 2, &mut j, &mut frame)?;
    if !j.progressive {
        // Sequential: a single scan, all components, in order.
        ensure!(params.ss == 0 && params.se == 63 && params.ah == 0 && params.al == 0, "balayage non séquentiel");
        ensure!(params.comps.len() == j.components.len(), "balayage partiel");
        for (k, &(ci, td, ta)) in params.comps.iter().enumerate() {
            ensure!(ci == k, "ordre des composantes inattendu");
            (j.components[k].td, j.components[k].ta) = (td, ta);
        }
    }
    j.first_scan = Some(params);
    j.header = d[..scan_start].to_vec();
    Ok(j)
}

/// Reads the segments from `i` up to a scan header (SOS) included, keeping
/// the tables of `j` up to date; returns the position following the scan
/// header and its parameters. Used for the file headers and for what
/// separates two scans of a progressive JPEG.
pub(crate) fn read_segments(d: &[u8], mut i: usize, j: &mut Jpeg, frame: &mut bool) -> Result<(usize, ScanParams)> {
    loop {
        ensure!(d.get(i) == Some(&0xFF), "marqueur JPEG attendu");
        let m = *d.get(i + 1).context("en-tête JPEG tronqué")?;
        if m == 0xFF {
            // Fill bytes between markers: not supported.
            bail!("remplissage entre marqueurs");
        }
        let len = be16(d, i + 2)?;
        ensure!(len >= 2, "segment JPEG invalide");
        let seg = d.get(i + 4..i + 2 + len).context("en-tête JPEG tronqué")?;
        match m {
            0xC0 | 0xC1 | 0xC2 => {
                ensure!(!*frame, "plusieurs images");
                *frame = true;
                j.progressive = m == 0xC2;
                ensure!(seg.len() >= 6 && seg[0] == 8, "précision non prise en charge");
                j.height = u16::from_be_bytes([seg[1], seg[2]]);
                j.width = u16::from_be_bytes([seg[3], seg[4]]);
                ensure!(j.width > 0 && j.height > 0, "hauteur définie plus loin (DNL) non prise en charge");
                let n = seg[5] as usize;
                ensure!((1..=4).contains(&n) && seg.len() == 6 + 3 * n, "composantes invalides");
                for k in 0..n {
                    let c = &seg[6 + 3 * k..9 + 3 * k];
                    let (h, v) = (c[1] >> 4, c[1] & 15);
                    ensure!((1..=4).contains(&h) && (1..=4).contains(&v) && c[2] < 4, "composante invalide");
                    j.components.push(Component { id: c[0], h, v, tq: c[2], td: 0, ta: 0 });
                }
            }
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => bail!("JPEG sans perte, hiérarchique ou arithmétique"),
            0xC4 => {
                let mut k = 0;
                while k < seg.len() {
                    let (class, id) = (seg[k] >> 4, seg[k] & 15);
                    ensure!(class < 2 && id < 4, "table de Huffman invalide");
                    let bits: [u8; 16] = seg.get(k + 1..k + 17).context("table de Huffman tronquée")?.try_into().unwrap();
                    let n: usize = bits.iter().map(|&b| b as usize).sum();
                    let values = seg.get(k + 17..k + 17 + n).context("table de Huffman tronquée")?.to_vec();
                    let h = Huffman { bits, values };
                    build_codes(&h)?;
                    if class == 0 {
                        j.dc_tables[id as usize] = Some(h);
                    } else {
                        j.ac_tables[id as usize] = Some(h);
                    }
                    k += 17 + n;
                }
            }
            0xDB => {
                let mut k = 0;
                while k < seg.len() {
                    let (pq, id) = (seg[k] >> 4, seg[k] & 15);
                    ensure!(id < 4 && pq < 2, "table de quantification invalide");
                    let mut q = [0u16; 64];
                    for (z, slot) in q.iter_mut().enumerate() {
                        *slot = if pq == 0 {
                            u16::from(*seg.get(k + 1 + z).context("table tronquée")?)
                        } else {
                            u16::from_be_bytes(seg.get(k + 1 + 2 * z..k + 3 + 2 * z).context("table tronquée")?.try_into().unwrap())
                        };
                    }
                    j.quant[id as usize] = Some(q);
                    k += 1 + 64 * (1 + pq as usize);
                }
            }
            0xDD => {
                ensure!(seg.len() == 2, "intervalle de reprise invalide");
                j.restart = u16::from_be_bytes([seg[0], seg[1]]);
            }
            0xDA => {
                ensure!(*frame, "balayage sans image");
                let n = *seg.first().context("balayage invalide")? as usize;
                ensure!((1..=4).contains(&n) && seg.len() == 1 + 2 * n + 3, "balayage invalide");
                let mut comps = Vec::with_capacity(n);
                for k in 0..n {
                    let (cid, t) = (seg[1 + 2 * k], seg[2 + 2 * k]);
                    let ci = j.components.iter().position(|c| c.id == cid).context("composante inconnue")?;
                    ensure!(comps.iter().all(|&(c, _, _)| c < ci), "ordre des composantes inattendu");
                    let (td, ta) = (t >> 4, t & 15);
                    ensure!(td < 4 && ta < 4, "table invalide");
                    comps.push((ci, td, ta));
                }
                let (ss, se, a) = (seg[1 + 2 * n], seg[2 + 2 * n], seg[3 + 2 * n]);
                let (ah, al) = (a >> 4, a & 15);
                ensure!(ss <= se && se <= 63 && ah <= 13 && al <= 13, "balayage invalide");
                return Ok((i + 2 + len, ScanParams { comps, ss, se, ah, al }));
            }
            0xD8 | 0xD9 | 0xD0..=0xD7 | 0x01 => bail!("marqueur inattendu dans l'en-tête"),
            _ => {}
        }
        i += 2 + len;
    }
}

/// Decodes the image data that follows the headers.
fn decode_scan(d: &[u8], j: &mut Jpeg) -> Result<()> {
    let scan_start = j.header.len();
    let decoders = |tables: &[Option<Huffman>; 4], id: u8| -> Result<Decoder> {
        Decoder::new(tables[id as usize].as_ref().context("table de Huffman manquante")?)
    };
    let dcd: Vec<Decoder> = j.components.iter().map(|c| decoders(&j.dc_tables, c.td)).collect::<Result<_>>()?;
    let acd: Vec<Decoder> = j.components.iter().map(|c| decoders(&j.ac_tables, c.ta)).collect::<Result<_>>()?;

    let (per_mcu, mcus) = j.layout();
    let total: usize = per_mcu.iter().sum::<usize>() * mcus;
    ensure!(total <= (d.len() - scan_start) * 8 + 64, "dimensions incohérentes avec les données");
    let interval = if j.restart == 0 { mcus } else { j.restart as usize };
    let (segments, end) = unstuff(d, scan_start)?;
    ensure!(d.get(end + 1) == Some(&0xD9), "fin d'image attendue après les données");
    ensure!(segments.len() == mcus.div_ceil(interval).max(1), "nombre de segments incohérent");
    j.blocks.reserve(total);
    let mut pred = vec![0i16; j.components.len()];
    for (si, seg) in segments.iter().enumerate() {
        let mut r = BitReader::new(seg);
        pred.iter_mut().for_each(|p| *p = 0);
        for _ in si * interval..((si + 1) * interval).min(mcus) {
            for (c, &n) in per_mcu.iter().enumerate() {
                for _ in 0..n {
                    let mut blk = [0i16; 64];
                    let s = dcd[c].decode(&mut r)?;
                    ensure!(s <= 11, "coefficient DC invalide");
                    let diff = extend(r.bits(s)?, s);
                    pred[c] = pred[c].wrapping_add(diff);
                    blk[0] = pred[c];
                    let mut k = 1;
                    let mut zrl = 0u8;
                    let mut ended = false;
                    while k < 64 {
                        let rs = acd[c].decode(&mut r)?;
                        let (run, size) = (rs >> 4, rs & 15);
                        if size == 0 {
                            if run == 15 {
                                zrl += 1;
                                k += 16;
                                continue;
                            }
                            ensure!(run == 0, "symbole AC invalide");
                            ended = true;
                            break;
                        }
                        ensure!(size <= 10, "coefficient AC invalide");
                        // "16 zeros" followed by a coefficient are not superfluous.
                        zrl = 0;
                        k += run as usize;
                        ensure!(k < 64, "bloc trop long");
                        blk[k] = extend(r.bits(size)?, size);
                        k += 1;
                    }
                    ensure!(k <= 64, "bloc trop long");
                    // Normal writing: no final "16 zeros", and an end of block only if the last
                    // non-zero coefficient is not the 63rd.
                    let last = (1..64).rev().find(|&k| blk[k] != 0).unwrap_or(0);
                    if zrl > 0 || ended != (last < 63) {
                        j.extra_zrl.push((j.blocks.len() as u32, zrl, ended));
                    }
                    j.blocks.push(blk);
                }
            }
        }
        j.pads.push(r.rest()?);
    }
    j.trailer = d[end..].to_vec();
    Ok(())
}

/// Rewrites the photo, byte for byte.
pub fn encode(j: &Jpeg) -> Result<Vec<u8>> {
    if j.progressive {
        return crate::jpegprog::encode(j);
    }
    let codes = |tables: &[Option<Huffman>; 4], id: u8| -> Result<[(u16, u8); 256]> {
        build_codes(tables[id as usize].as_ref().context("table de Huffman manquante")?)
    };
    let dcc: Vec<_> = j.components.iter().map(|c| codes(&j.dc_tables, c.td)).collect::<Result<_>>()?;
    let acc: Vec<_> = j.components.iter().map(|c| codes(&j.ac_tables, c.ta)).collect::<Result<_>>()?;
    let put = |w: &mut BitWriter, code: (u16, u8)| -> Result<()> {
        ensure!(code.1 > 0, "symbole absent de la table de Huffman");
        w.put(code.0, code.1);
        Ok(())
    };
    let (per_mcu, mcus) = j.layout();
    ensure!(j.blocks.len() == per_mcu.iter().sum::<usize>() * mcus, "nombre de blocs incohérent");
    let interval = if j.restart == 0 { mcus } else { j.restart as usize };
    ensure!(j.pads.len() == mcus.div_ceil(interval).max(1), "remplissage incohérent");

    let mut out = j.header.clone();
    let mut w = BitWriter::default();
    let mut pred = vec![0i16; j.components.len()];
    let mut extra = j.extra_zrl.iter().peekable();
    let mut pads = j.pads.iter();
    let mut b = 0usize;
    let pad = |w: &mut BitWriter, p: (u8, u8)| -> Result<()> {
        ensure!(p.1 < 8 && u16::from(p.0) >> p.1 == 0, "remplissage invalide");
        w.put(u16::from(p.0), p.1);
        ensure!(w.aligned(), "remplissage incohérent");
        Ok(())
    };
    for mcu in 0..mcus {
        if mcu > 0 && mcu % interval == 0 {
            pad(&mut w, *pads.next().unwrap())?;
            out.append(&mut w.out);
            out.extend_from_slice(&[0xFF, 0xD0 + ((mcu / interval - 1) % 8) as u8]);
            pred.iter_mut().for_each(|p| *p = 0);
        }
        for (c, &n) in per_mcu.iter().enumerate() {
            for _ in 0..n {
                let blk = &j.blocks[b];
                let diff = blk[0].wrapping_sub(pred[c]);
                pred[c] = blk[0];
                let (s, bits) = category(diff);
                ensure!(s <= 11, "coefficient DC invalide");
                put(&mut w, dcc[c][s as usize])?;
                w.put(bits, s);
                let last = (1..64).rev().find(|&k| blk[k] != 0).unwrap_or(0);
                let mut run = 0;
                for &v in &blk[1..=last] {
                    if v == 0 {
                        run += 1;
                        continue;
                    }
                    while run >= 16 {
                        put(&mut w, acc[c][0xF0])?;
                        run -= 16;
                    }
                    let (s, bits) = category(v);
                    ensure!((1..=10).contains(&s), "coefficient AC invalide");
                    put(&mut w, acc[c][(run << 4 | s) as usize])?;
                    w.put(bits, s);
                    run = 0;
                }
                let (zrl, eob) = match extra.peek() {
                    Some(&&(nb, z, e)) if nb as usize == b => {
                        extra.next();
                        (z, e)
                    }
                    _ => (0, last < 63),
                };
                for _ in 0..zrl {
                    put(&mut w, acc[c][0xF0])?;
                }
                if eob {
                    put(&mut w, acc[c][0x00])?;
                }
                b += 1;
            }
        }
    }
    pad(&mut w, *pads.next().unwrap())?;
    ensure!(pads.next().is_none() && extra.next().is_none(), "exceptions en trop");
    out.append(&mut w.out);
    out.extend_from_slice(&j.trailer);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories() {
        for v in [-2047i16, -1024, -3, -1, 0, 1, 2, 7, 8, 1023, 2047] {
            let (s, bits) = category(v);
            assert_eq!(extend(bits, s), v);
        }
    }
}
