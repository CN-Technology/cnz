//! Progressive JPEGs (stage 5): exact reading and rewriting.
//!
//! A progressive JPEG writes the image in several **scans**: first the DC
//! coefficients (first pass, then one-bit refinements), then bands of AC
//! coefficients, one component at a time (first pass, then refinements). In
//! the end, the coefficients are the same as those of a sequential JPEG: the
//! model of jpegmodel.rs is used as is. To rewrite the file byte for byte,
//! we keep, for each scan:
//!
//! - the bytes that precede it (Huffman tables, restart interval, scan
//!   header), copied as they are;
//! - the fill bits of each segment;
//! - the length of each run of empty blocks (EOBRUN), which the encoder
//!   chooses freely.
//!
//! The algorithms follow the standard (ITU T.81, annex G) and libjpeg.

use anyhow::{bail, ensure, Context, Result};

use crate::jpeg::{build_codes, category, extend, read_segments, unstuff, BitReader, BitWriter, Decoder, Huffman, Jpeg, ScanParams};

/// What we keep of a scan.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Scan {
    /// Bytes between the end of the previous scan's data and the end of this
    /// scan's header (empty for the first one, included in the photo headers).
    pub prefix: Vec<u8>,
    /// Fill bits at the end of each segment (value, count).
    pub pads: Vec<(u8, u8)>,
    /// Length of each run of empty blocks, in order (empty if `standard`).
    pub eobruns: Vec<u32>,
    /// The runs follow libjpeg's rule (as long as possible, at most 32,767
    /// blocks, and in refinement as soon as more than 937 correction bits are
    /// waiting): no need to keep their lengths.
    pub standard: bool,
}

/// Block grids: for each component, the complete grid (aligned on MCUs) and
/// the real part (single-component scans).
struct Grids {
    full: Vec<(usize, usize)>,
    real: Vec<(usize, usize)>,
    mcus: (usize, usize),
}

fn grids(j: &Jpeg) -> Grids {
    let hmax = j.components.iter().map(|c| c.h).max().unwrap_or(1) as usize;
    let vmax = j.components.iter().map(|c| c.v).max().unwrap_or(1) as usize;
    let real: Vec<(usize, usize)> = j
        .components
        .iter()
        .map(|c| {
            (
                (j.width as usize * c.h as usize).div_ceil(hmax).div_ceil(8),
                (j.height as usize * c.v as usize).div_ceil(vmax).div_ceil(8),
            )
        })
        .collect();
    if j.components.len() == 1 {
        return Grids { full: real.clone(), real, mcus: (0, 0) };
    }
    let mcus = ((j.width as usize).div_ceil(8 * hmax), (j.height as usize).div_ceil(8 * vmax));
    let full = j.components.iter().map(|c| (mcus.0 * c.h as usize, mcus.1 * c.v as usize)).collect();
    Grids { full, real, mcus }
}

/// Block order of a sequential JPEG (the model's): for each block,
/// (component, x, y) in the complete grid.
fn sequential_order(j: &Jpeg, g: &Grids) -> Vec<(usize, usize, usize)> {
    if j.components.len() == 1 {
        let (w, h) = g.full[0];
        return (0..w * h).map(|i| (0, i % w, i / w)).collect();
    }
    let mut pos = Vec::new();
    for y in 0..g.mcus.1 {
        for x in 0..g.mcus.0 {
            for (ci, c) in j.components.iter().enumerate() {
                for by in 0..c.v as usize {
                    for bx in 0..c.h as usize {
                        pos.push((ci, x * c.h as usize + bx, y * c.v as usize + by));
                    }
                }
            }
        }
    }
    pos
}

/// Blocks visited by a scan, MCU by MCU: (component, x, y).
fn scan_order(j: &Jpeg, g: &Grids, p: &ScanParams) -> Vec<Vec<(usize, usize, usize)>> {
    if p.comps.len() == 1 {
        // A single component: one MCU = one block, real part only.
        let ci = p.comps[0].0;
        let (w, h) = if j.components.len() == 1 { g.full[0] } else { g.real[ci] };
        return (0..w * h).map(|i| vec![(ci, i % w, i / w)]).collect();
    }
    let mut mcus = Vec::new();
    for y in 0..g.mcus.1 {
        for x in 0..g.mcus.0 {
            let mut m = Vec::new();
            for &(ci, _, _) in &p.comps {
                let c = &j.components[ci];
                for by in 0..c.v as usize {
                    for bx in 0..c.h as usize {
                        m.push((ci, x * c.h as usize + bx, y * c.v as usize + by));
                    }
                }
            }
            mcus.push(m);
        }
    }
    mcus
}

/// Do the segments that follow `i` lead to a new scan (and not to the end of
/// the image)?
fn another_scan(d: &[u8], mut i: usize) -> bool {
    loop {
        match (d.get(i), d.get(i + 1)) {
            (Some(0xFF), Some(0xDA)) => return true,
            (Some(0xFF), Some(&m)) if m != 0xD9 && !(0xD0..=0xD8).contains(&m) && m != 0x01 && m != 0xFF => {
                let Some(len) = d.get(i + 2..i + 4).map(|b| usize::from(u16::from_be_bytes([b[0], b[1]]))) else { return false };
                if len < 2 {
                    return false;
                }
                i += 2 + len;
            }
            _ => return false,
        }
    }
}

/// Copy of the photo's state (tables, dimensions), without the blocks.
fn state(j: &Jpeg) -> Jpeg {
    Jpeg {
        header: Vec::new(),
        width: j.width,
        height: j.height,
        components: j.components.clone(),
        dc_tables: j.dc_tables.clone(),
        ac_tables: j.ac_tables.clone(),
        quant: j.quant,
        restart: j.restart,
        blocks: Vec::new(),
        pads: Vec::new(),
        extra_zrl: Vec::new(),
        trailer: Vec::new(),
        progressive: j.progressive,
        scans: Vec::new(),
        first_scan: None,
    }
}

fn table<'a>(t: &'a [Option<Huffman>; 4], id: u8) -> Result<&'a Huffman> {
    t[id as usize].as_ref().context("table de Huffman manquante")
}

/// Value of an AC coefficient reduced by `al` bits (towards zero, like libjpeg).
fn shrink(c: i16, al: u8) -> i32 {
    let c = i32::from(c);
    if c < 0 {
        -((-c) >> al)
    } else {
        c >> al
    }
}

/// Decodes the scans that follow the headers; fills the blocks (in
/// sequential order) and the description of the scans.
pub fn decode(d: &[u8], j: &mut Jpeg) -> Result<()> {
    let g = grids(j);
    let total: usize = g.full.iter().map(|&(w, h)| w * h).sum();
    ensure!(total <= d.len().saturating_mul(8).saturating_add(64), "dimensions incohérentes avec les données");
    let mut coefs: Vec<Vec<[i16; 64]>> = g.full.iter().map(|&(w, h)| vec![[0i16; 64]; w * h]).collect();
    // State that evolves from one scan to the next (tables, restart interval).
    let mut st = state(j);
    let mut frame = true;
    let mut params = j.first_scan.clone().context("balayage absent")?;
    let mut pos = j.header.len();
    let mut prefix = Vec::new();
    loop {
        let mut scan = Scan { prefix: std::mem::take(&mut prefix), ..Default::default() };
        let (segments, end) = unstuff(d, pos)?;
        decode_scan(&st, &g, &params, &segments, &mut coefs, &mut scan)?;
        j.scans.push(scan);
        ensure!(j.scans.len() <= 1000, "trop de balayages");
        // No more scans (end of image, possibly preceded by a comment or an unused
        // table): everything left is the end.
        if !another_scan(d, end) {
            j.trailer = d[end..].to_vec();
            break;
        }
        let (after, p) = read_segments(d, end, &mut st, &mut frame)?;
        prefix = d[end..after].to_vec();
        params = p;
        pos = after;
    }
    j.blocks = sequential_order(j, &g).into_iter().map(|(ci, x, y)| coefs[ci][y * g.full[ci].0 + x]).collect();
    Ok(())
}

fn decode_scan(
    st: &Jpeg,
    g: &Grids,
    p: &ScanParams,
    segments: &[Vec<u8>],
    coefs: &mut [Vec<[i16; 64]>],
    scan: &mut Scan,
) -> Result<()> {
    let order = scan_order(st, g, p);
    let interval = if st.restart == 0 { order.len() } else { st.restart as usize };
    ensure!(segments.len() == order.len().div_ceil(interval.max(1)).max(1), "nombre de segments incohérent");
    let dc: Vec<Option<Decoder>> = p
        .comps
        .iter()
        .map(|&(_, td, _)| if p.ss == 0 && p.ah == 0 { table(&st.dc_tables, td).and_then(Decoder::new).map(Some) } else { Ok(None) })
        .collect::<Result<_>>()?;
    let ac: Vec<Option<Decoder>> = p
        .comps
        .iter()
        .map(|&(_, _, ta)| if p.ss > 0 { table(&st.ac_tables, ta).and_then(Decoder::new).map(Some) } else { Ok(None) })
        .collect::<Result<_>>()?;
    ensure!(p.ss > 0 || p.se == 0, "balayage DC invalide");
    ensure!(p.ss == 0 || p.comps.len() == 1, "balayage AC à plusieurs composantes");
    let (ss, se, al) = (p.ss as usize, p.se as usize, p.al);
    for (si, seg) in segments.iter().enumerate() {
        let mut r = BitReader::new(seg);
        let mut pred = vec![0i32; p.comps.len()];
        let mut eobrun = 0u32;
        for mcu in &order[si * interval..((si + 1) * interval).min(order.len())] {
            for &(ci, x, y) in mcu {
                let k_comp = p.comps.iter().position(|c| c.0 == ci).unwrap();
                let blk = &mut coefs[ci][y * g.full[ci].0 + x];
                if ss == 0 {
                    if p.ah == 0 {
                        let s = dc[k_comp].as_ref().unwrap().decode(&mut r)?;
                        ensure!(s <= 11, "coefficient DC invalide");
                        pred[k_comp] += i32::from(extend(r.bits(s)?, s));
                        blk[0] = (pred[k_comp] << al) as i16;
                    } else if r.bits(1)? == 1 {
                        blk[0] |= 1 << al;
                    }
                    continue;
                }
                let dec = ac[k_comp].as_ref().unwrap();
                if p.ah == 0 {
                    // First pass of an AC band.
                    if eobrun > 0 {
                        eobrun -= 1;
                        continue;
                    }
                    let mut k = ss;
                    while k <= se {
                        let rs = dec.decode(&mut r)?;
                        let (run, s) = (usize::from(rs >> 4), rs & 15);
                        if s == 0 {
                            if run < 15 {
                                eobrun = (1u32 << run) + u32::from(r.bits(run as u8)?);
                                scan.eobruns.push(eobrun);
                                eobrun -= 1;
                                break;
                            }
                            k += 16;
                            continue;
                        }
                        k += run;
                        ensure!(k <= se, "bloc trop long");
                        blk[k] = (i32::from(extend(r.bits(s)?, s)) << al) as i16;
                        k += 1;
                    }
                } else {
                    // Refinement of an AC band (standard G.1.2.3, libjpeg).
                    let (p1, m1) = (1i16 << al, -1i16 << al);
                    let mut k = ss;
                    if eobrun == 0 {
                        while k <= se {
                            let rs = dec.decode(&mut r)?;
                            let (mut run, s) = (i32::from(rs >> 4), rs & 15);
                            let mut value = 0i16;
                            if s != 0 {
                                ensure!(s == 1, "affinage invalide");
                                value = if r.bits(1)? == 1 { p1 } else { m1 };
                            } else if run != 15 {
                                eobrun = 1u32 << run;
                                if run > 0 {
                                    eobrun += u32::from(r.bits(run as u8)?);
                                }
                                scan.eobruns.push(eobrun);
                                break;
                            }
                            loop {
                                let c = &mut blk[k];
                                if *c != 0 {
                                    if r.bits(1)? == 1 && (*c & p1) == 0 {
                                        *c += if *c >= 0 { p1 } else { m1 };
                                    }
                                } else {
                                    run -= 1;
                                    if run < 0 {
                                        break;
                                    }
                                }
                                k += 1;
                                if k > se {
                                    break;
                                }
                            }
                            if value != 0 {
                                ensure!(k <= se, "affinage invalide");
                                blk[k] = value;
                            }
                            k += 1;
                        }
                    }
                    if eobrun > 0 {
                        while k <= se {
                            let c = &mut blk[k];
                            if *c != 0 && r.bits(1)? == 1 && (*c & p1) == 0 {
                                *c += if *c >= 0 { p1 } else { m1 };
                            }
                            k += 1;
                        }
                        eobrun -= 1;
                    }
                }
            }
        }
        ensure!(eobrun == 0, "série de blocs vides au-delà du segment");
        scan.pads.push(r.rest()?);
    }
    Ok(())
}

/// Writing a scan: bits, pending run of empty blocks and its deferred
/// correction bits, expected run lengths.
struct Emitter<'a> {
    w: BitWriter,
    ac: [(u16, u8); 256],
    eobrun: u32,
    deferred: Vec<u8>,
    /// Expected lengths (None: libjpeg's rule).
    runs: Option<std::slice::Iter<'a, u32>>,
    /// Lengths actually written.
    written: Vec<u32>,
}

impl Emitter<'_> {
    fn code(&mut self, sym: u8) -> Result<()> {
        let (c, l) = self.ac[sym as usize];
        ensure!(l > 0, "symbole absent de la table de Huffman");
        self.w.put(c, l);
        Ok(())
    }

    /// Writes the pending run (its length must be the file's).
    fn flush_run(&mut self) -> Result<()> {
        if self.eobrun == 0 {
            return Ok(());
        }
        if let Some(runs) = &mut self.runs {
            ensure!(runs.next() == Some(&self.eobrun), "série de blocs vides inattendue");
        }
        self.written.push(self.eobrun);
        let n = 31 - self.eobrun.leading_zeros();
        self.code((n as u8) << 4)?;
        if n > 0 {
            self.w.put((self.eobrun - (1 << n)) as u16, n as u8);
        }
        for b in std::mem::take(&mut self.deferred) {
            self.w.put(u16::from(b), 1);
        }
        self.eobrun = 0;
        Ok(())
    }

    /// One more block in the run; it is written if it has the length chosen by
    /// the original encoder.
    fn extend_run(&mut self) -> Result<()> {
        self.eobrun += 1;
        let flush = match &self.runs {
            Some(runs) => runs.as_slice().first() == Some(&self.eobrun),
            None => self.eobrun == 0x7FFF || self.deferred.len() > 937,
        };
        if flush {
            self.flush_run()?;
        }
        Ok(())
    }
}

/// Rewrites a progressive photo, byte for byte.
pub fn encode(j: &Jpeg) -> Result<Vec<u8>> {
    Ok(encode_with(j, false)?.0)
}

/// Marks as `standard` the scans whose runs of empty blocks follow libjpeg's
/// rule (their lengths then need not be kept).
pub fn mark_standard(j: &mut Jpeg) -> Result<()> {
    let (_, runs) = encode_with(j, true)?;
    for (sc, r) in j.scans.iter_mut().zip(runs) {
        // A failure in trial mode only means "not standard".
        if r.as_ref() == Some(&sc.eobruns) {
            sc.standard = true;
            sc.eobruns.clear();
        }
    }
    Ok(())
}

/// `all_standard`: try to write every run according to libjpeg's rule (to
/// compare); also returns the runs written in each scan (None: the trial
/// failed for that scan).
fn encode_with(j: &Jpeg, all_standard: bool) -> Result<(Vec<u8>, Vec<Option<Vec<u32>>>)> {
    let g = grids(j);
    let order = sequential_order(j, &g);
    ensure!(order.len() == j.blocks.len(), "nombre de blocs incohérent");
    let mut coefs: Vec<Vec<[i16; 64]>> = g.full.iter().map(|&(w, h)| vec![[0i16; 64]; w * h]).collect();
    for (&(ci, x, y), b) in order.iter().zip(&j.blocks) {
        coefs[ci][y * g.full[ci].0 + x] = *b;
    }
    let mut out = j.header.clone();
    let mut st = state(j);
    let mut frame = true;
    let mut params = j.first_scan.clone().context("balayage absent")?;
    let mut all_runs = Vec::with_capacity(j.scans.len());
    for (n, scan) in j.scans.iter().enumerate() {
        if n > 0 {
            let (after, p) = read_segments(&scan.prefix, 0, &mut st, &mut frame).map_err(|_| anyhow::anyhow!("balayage invalide"))?;
            ensure!(after == scan.prefix.len(), "balayage invalide");
            out.extend_from_slice(&scan.prefix);
            params = p;
        }
        if all_standard {
            let mut scratch = Vec::new();
            all_runs.push(encode_scan(&st, &g, &params, &coefs, scan, true, &mut scratch).ok());
        } else {
            all_runs.push(Some(encode_scan(&st, &g, &params, &coefs, scan, scan.standard, &mut out)?));
        }
    }
    ensure!(j.trailer.first() == Some(&0xFF), "fin d'image attendue");
    out.extend_from_slice(&j.trailer);
    Ok((out, all_runs))
}

fn encode_scan(
    st: &Jpeg,
    g: &Grids,
    p: &ScanParams,
    coefs: &[Vec<[i16; 64]>],
    scan: &Scan,
    standard: bool,
    out: &mut Vec<u8>,
) -> Result<Vec<u32>> {
    let order = scan_order(st, g, p);
    let interval = if st.restart == 0 { order.len() } else { st.restart as usize };
    let nseg = order.len().div_ceil(interval.max(1)).max(1);
    ensure!(scan.pads.len() == nseg, "remplissage incohérent");
    let dc: Vec<[(u16, u8); 256]> = p
        .comps
        .iter()
        .map(|&(_, td, _)| if p.ss == 0 && p.ah == 0 { build_codes(table(&st.dc_tables, td)?) } else { Ok([(0, 0); 256]) })
        .collect::<Result<_>>()?;
    let ac = match p.comps.first() {
        Some(&(_, _, ta)) if p.ss > 0 => build_codes(table(&st.ac_tables, ta)?)?,
        _ => [(0, 0); 256],
    };
    let runs = (!standard).then(|| scan.eobruns.iter());
    let mut e = Emitter { w: BitWriter::default(), ac, eobrun: 0, deferred: Vec::new(), runs, written: Vec::new() };
    let (ss, se, al) = (p.ss as usize, p.se as usize, p.al);
    for si in 0..nseg {
        let mut pred = vec![0i32; p.comps.len()];
        for mcu in &order[si * interval..((si + 1) * interval).min(order.len())] {
            for &(ci, x, y) in mcu {
                let k_comp = p.comps.iter().position(|c| c.0 == ci).unwrap();
                let blk = &coefs[ci][y * g.full[ci].0 + x];
                if ss == 0 {
                    if p.ah == 0 {
                        let v = i32::from(blk[0]) >> al;
                        let diff = v - pred[k_comp];
                        pred[k_comp] = v;
                        let (s, bits) = category(i16::try_from(diff).map_err(|_| anyhow::anyhow!("coefficient DC invalide"))?);
                        ensure!(s <= 11, "coefficient DC invalide");
                        let (c, l) = dc[k_comp][s as usize];
                        ensure!(l > 0, "symbole absent de la table de Huffman");
                        e.w.put(c, l);
                        e.w.put(bits, s);
                    } else {
                        e.w.put(((i32::from(blk[0]) >> al) & 1) as u16, 1);
                    }
                    continue;
                }
                if p.ah == 0 {
                    // First pass of an AC band.
                    let last = (ss..=se).rev().find(|&k| shrink(blk[k], al) != 0);
                    let Some(last) = last else {
                        e.extend_run()?;
                        continue;
                    };
                    let mut run = 0u8;
                    for k in ss..=last {
                        let v = shrink(blk[k], al);
                        if v == 0 {
                            run += 1;
                            continue;
                        }
                        e.flush_run()?;
                        while run > 15 {
                            e.code(0xF0)?;
                            run -= 16;
                        }
                        let (s, bits) = category(v as i16);
                        ensure!((1..=14).contains(&s), "coefficient AC invalide");
                        e.code(run << 4 | s)?;
                        e.w.put(bits, s);
                        run = 0;
                    }
                    if last < se {
                        e.extend_run()?;
                    }
                } else {
                    // Refinement of an AC band (libjpeg, encode_mcu_AC_refine).
                    let abs: Vec<i32> = (ss..=se).map(|k| i32::from(blk[k]).abs() >> al).collect();
                    let eob = abs.iter().rposition(|&a| a == 1).map(|i| i + ss);
                    let mut run = 0u32;
                    let mut buffered: Vec<u8> = Vec::new();
                    for k in ss..=se {
                        let t = abs[k - ss];
                        if t == 0 {
                            run += 1;
                            continue;
                        }
                        while run > 15 && eob.is_some_and(|e| k <= e) {
                            e.flush_run()?;
                            e.code(0xF0)?;
                            run -= 16;
                            for b in buffered.drain(..) {
                                e.w.put(u16::from(b), 1);
                            }
                        }
                        if t > 1 {
                            buffered.push((t & 1) as u8);
                            continue;
                        }
                        e.flush_run()?;
                        e.code((run as u8) << 4 | 1)?;
                        e.w.put(u16::from(blk[k] >= 0), 1);
                        for b in buffered.drain(..) {
                            e.w.put(u16::from(b), 1);
                        }
                        run = 0;
                    }
                    if run > 0 || !buffered.is_empty() {
                        e.deferred.extend(buffered);
                        e.extend_run()?;
                    }
                }
            }
        }
        e.flush_run()?;
        let (v, n) = scan.pads[si];
        ensure!(n < 8 && u16::from(v) >> n == 0, "remplissage invalide");
        e.w.put(u16::from(v), n);
        ensure!(e.w.aligned(), "remplissage incohérent");
        out.append(&mut e.w.out);
        if si + 1 < nseg {
            out.extend_from_slice(&[0xFF, 0xD0 + (si % 8) as u8]);
        }
    }
    if e.runs.as_mut().is_some_and(|r| r.next().is_some()) {
        bail!("séries de blocs vides en trop");
    }
    Ok(e.written)
}
