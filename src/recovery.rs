//! Recovery data: a few % of redundancy (Reed-Solomon codes) that make it
//! possible to **rebuild** the damaged parts of an archive.
//!
//! The protected area (everything that precedes the recovery data) is cut
//! into fixed-size slices, each with its CRC-32C. The slices are spread over
//! **interleaved** groups: slice i goes into group i % G. A contiguous
//! damaged area thus touches each group a little instead of overwhelming a
//! single one. Each group gets its own recovery slices: with r recovery
//! slices, a group can lose any r slices.
//!
//! Everything is done on the stored bytes: an encrypted archive can be
//! repaired without the password.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;

use crate::format::*;
use crate::progress::Progress;

const TABLE_VERSION: u64 = 1;
const MIN_SLICE: u64 = 4096;
/// At most ~65,536 slices: the CRC table stays small (256 KB).
const MAX_SLICES: u64 = 65_536;
/// A group fits in memory: at most 4,096 slices and 64 MB.
const MAX_GROUP_SLICES: u64 = 4096;
const MAX_GROUP_BYTES: u64 = 64 << 20;

/// Split of the protected area into slices and groups.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub protected_len: u64,
    pub slice_size: u64,
    pub groups: u64,
    /// Redundancy per thousand (30 = 3 %).
    pub permille: u64,
}

impl Layout {
    pub fn new(protected_len: u64, percent: u32) -> Self {
        let slice_size = protected_len.div_ceil(MAX_SLICES).max(MIN_SLICE).next_multiple_of(MIN_SLICE);
        let slices = protected_len.div_ceil(slice_size);
        let per_group = MAX_GROUP_SLICES.min((MAX_GROUP_BYTES / slice_size).max(1));
        let groups = slices.div_ceil(per_group).max(1);
        Self { protected_len, slice_size, groups, permille: u64::from(percent) * 10 }
    }

    /// Split that guarantees the rebuild of `blocks` whole blocks of at most
    /// `largest` bytes, and at least `percent` % of redundancy.
    pub fn for_blocks(protected_len: u64, percent: u32, blocks: u32, largest: u64) -> Self {
        let mut layout = Self::new(protected_len, percent);
        if blocks > 0 {
            // A destroyed block covers at most ⌈size / S⌉ + 1 slices (it may spill over
            // one slice at each end), spread over the G groups.
            let per_block = (largest.div_ceil(layout.slice_size) + 1).div_ceil(layout.groups);
            let needed = u64::from(blocks) * per_block;
            // The last group is the smallest: it is the one that sets the rate.
            let smallest = layout.group_len(layout.groups - 1).max(1);
            layout.permille = layout.permille.max((needed * 1000).div_ceil(smallest).min(1000));
        }
        layout
    }

    pub fn slices(&self) -> u64 {
        self.protected_len.div_ceil(self.slice_size)
    }

    /// Numbers of the slices of group g: g, g + G, g + 2G…
    fn members(&self, g: u64) -> impl Iterator<Item = u64> {
        (g..self.slices()).step_by(self.groups as usize)
    }

    fn group_len(&self, g: u64) -> u64 {
        self.slices().saturating_sub(g).div_ceil(self.groups)
    }

    fn recovery_count(&self, g: u64) -> u64 {
        let k = self.group_len(g);
        if k == 0 {
            0
        } else {
            (k * self.permille).div_ceil(1000).max(1)
        }
    }

    /// Position (in slices) of the first recovery slice of each group.
    fn recovery_starts(&self) -> Vec<u64> {
        let mut starts = Vec::with_capacity(self.groups as usize + 1);
        let mut acc = 0;
        for g in 0..self.groups {
            starts.push(acc);
            acc += self.recovery_count(g);
        }
        starts.push(acc);
        starts
    }

    fn slice_range(&self, i: u64) -> (u64, usize) {
        let start = i * self.slice_size;
        (start, (self.protected_len - start).min(self.slice_size) as usize)
    }
}

/// Recovery table: split, CRC of each slice and of each recovery slice, and
/// their position in the archive.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub layout: Layout,
    pub data_offset: u64,
    pub slice_crcs: Vec<u32>,
    pub shard_crcs: Vec<u32>,
}

impl Table {
    pub fn encode(&self) -> Vec<u8> {
        let l = &self.layout;
        let mut o = Vec::new();
        for v in [TABLE_VERSION, l.protected_len, l.slice_size, l.groups, l.permille, self.data_offset] {
            put_varint(&mut o, v);
        }
        for crc in self.slice_crcs.iter().chain(&self.shard_crcs) {
            o.extend_from_slice(&crc.to_le_bytes());
        }
        o
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(buf);
        if c.varint()? != TABLE_VERSION {
            bail!("table de récupération d'une version inconnue");
        }
        let layout = Layout {
            protected_len: c.varint()?,
            slice_size: c.varint()?,
            groups: c.varint()?,
            permille: c.varint()?,
        };
        let data_offset = c.varint()?;
        // Each value is bounded as when writing: a booby-trapped table must not
        // make billions of slices be computed.
        if layout.slice_size == 0
            || layout.slice_size % 2 != 0
            || layout.slice_size > MAX_GROUP_BYTES
            || layout.groups == 0
            || layout.groups > layout.slices().max(1)
            || layout.slices() > 4 * MAX_SLICES
            || layout.group_len(0) > MAX_GROUP_SLICES
            || layout.permille == 0
            || layout.permille > 1000
        {
            bail!("table de récupération incohérente");
        }
        let nslices = layout.slices() as usize;
        let nshards = *layout.recovery_starts().last().unwrap() as usize;
        if c.remaining() != 4 * (nslices + nshards) {
            bail!("table de récupération tronquée");
        }
        let mut crcs = Vec::with_capacity(nslices + nshards);
        for _ in 0..nslices + nshards {
            crcs.push(u32_at(c.bytes(4)?, 0));
        }
        let shard_crcs = crcs.split_off(nslices);
        Ok(Self { layout, data_offset, slice_crcs: crcs, shard_crcs })
    }

    fn block(&self) -> (BlockHeader, Vec<u8>) {
        let payload = self.encode();
        let header = BlockHeader {
            kind: BlockKind::Recovery,
            codec: Codec::Store,
            raw_len: payload.len() as u64,
            comp_len: payload.len() as u64,
            payload_crc: crc32c::crc32c(&payload),
        };
        (header, payload)
    }
}

/// Reads a slice, padded with zeros up to the size of a slice. Returns None
/// if it is unreadable (truncated file).
fn read_slice(file: &File, file_len: u64, layout: &Layout, i: u64) -> Option<(Vec<u8>, usize)> {
    let (start, len) = layout.slice_range(i);
    if start + len as u64 > file_len {
        return None;
    }
    let mut buf = vec![0u8; layout.slice_size as usize];
    read_exact_at(file, &mut buf[..len], start).ok()?;
    Some((buf, len))
}

/// Computes the recovery data of the area [0, protected_len) of `source` and
/// writes it into `out` (which starts at position `pos`): recovery slices,
/// table, then a copy of the table.
/// Returns (table position, new position).
pub fn write(source: &File, layout: Layout, out: &mut dyn Write, mut pos: u64, progress: &Progress) -> Result<(u64, u64)> {
    let file_len = source.metadata()?.len();
    let mut table = Table {
        layout,
        data_offset: pos,
        slice_crcs: vec![0; layout.slices() as usize],
        shard_crcs: Vec::new(),
    };
    // Groups are computed in batches on all cores, then written in order.
    let batch = rayon::current_num_threads().max(1) * 2;
    let groups: Vec<u64> = (0..layout.groups).collect();
    for chunk in groups.chunks(batch) {
        progress.check()?;
        let done: Vec<Result<(Vec<(u64, u32)>, Vec<Vec<u8>>)>> = chunk
            .par_iter()
            .map(|&g| {
                let mut crcs = Vec::new();
                let mut originals = Vec::new();
                for i in layout.members(g) {
                    let (buf, len) = read_slice(source, file_len, &layout, i).context("archive illisible")?;
                    crcs.push((i, crc32c::crc32c(&buf[..len])));
                    originals.push(buf);
                }
                let shards = reed_solomon_simd::encode(originals.len(), layout.recovery_count(g) as usize, &originals)
                    .map_err(|e| anyhow::anyhow!("calcul de récupération impossible : {e}"))?;
                Ok((crcs, shards))
            })
            .collect();
        for group in done {
            let (crcs, shards) = group?;
            progress.advance(crcs.len() as u64 * layout.slice_size);
            for (i, crc) in crcs {
                table.slice_crcs[i as usize] = crc;
            }
            for shard in shards {
                table.shard_crcs.push(crc32c::crc32c(&shard));
                out.write_all(&shard)?;
                pos += shard.len() as u64;
            }
        }
    }
    // The table, twice: without it, the recovery slices are unusable.
    let (header, payload) = table.block();
    let table_offset = pos;
    for _ in 0..2 {
        out.write_all(&header.encode())?;
        out.write_all(&payload)?;
        pos += (BLOCK_HEADER_LEN + payload.len()) as u64;
    }
    Ok((table_offset, pos))
}

/// Finds the recovery table: through the footer, otherwise by searching the
/// whole file (the header or the footer may themselves be damaged).
fn find_table(file: &File, file_len: u64) -> Option<Table> {
    let try_at = |offset: u64| -> Option<Table> {
        let (header, payload) = read_block_at(file, file_len, offset).ok()?;
        (header.kind == BlockKind::Recovery).then(|| Table::decode(&payload).ok()).flatten()
    };
    if file_len >= FOOTER_LEN as u64 {
        let mut f = [0u8; FOOTER_LEN];
        if read_exact_at(file, &mut f, file_len - FOOTER_LEN as u64).is_ok() {
            if let Ok(footer) = Footer::decode(&f) {
                if footer.recovery_offset != 0 {
                    if let Some(t) = try_at(footer.recovery_offset) {
                        return Some(t);
                    }
                }
            }
        }
    }
    // Search for the block signature in the whole file, starting from the end.
    let window = 1 << 20;
    let mut end = file_len;
    let mut buf = vec![0u8; window + 3];
    while end > 0 {
        let start = end.saturating_sub(window as u64);
        let n = ((end - start) as usize + 3).min((file_len - start) as usize);
        if read_exact_at(file, &mut buf[..n], start).is_err() {
            break;
        }
        let hits: Vec<usize> = buf[..n].windows(4).enumerate().filter(|(_, w)| *w == BLOCK_MAGIC).map(|(i, _)| i).collect();
        for &i in hits.iter().rev() {
            if let Some(t) = try_at(start + i as u64) {
                return Some(t);
            }
        }
        end = start;
    }
    None
}

#[derive(Debug, Default)]
pub struct RepairReport {
    /// The archive contains readable recovery data.
    pub has_recovery: bool,
    pub slices: u64,
    pub damaged: u64,
    pub repaired: u64,
    /// Slices damaged beyond what recovery can rebuild.
    pub lost: u64,
    /// Numbers of the damaged slices, and of those that cannot be rebuilt.
    pub damaged_slices: Vec<u64>,
    pub lost_slices: Vec<u64>,
}

/// Checks the archive slice by slice and, if `output` is given, writes a
/// repaired copy. Without `output`, only diagnoses.
pub fn repair(path: &Path, output: Option<&Path>) -> Result<RepairReport> {
    let file = File::open(path).with_context(|| format!("impossible d'ouvrir {}", path.display()))?;
    let file_len = file.metadata()?.len();
    let Some(table) = find_table(&file, file_len) else {
        return Ok(RepairReport::default());
    };
    let layout = table.layout;
    let starts = layout.recovery_starts();
    let mut report = RepairReport { has_recovery: true, slices: layout.slices(), ..Default::default() };

    let results: Vec<Result<(Vec<u64>, bool, Vec<(u64, Vec<u8>)>)>> = (0..layout.groups)
        .into_par_iter()
        .map(|g| {
            // Healthy and damaged slices of the group.
            let mut originals = Vec::new();
            let mut damaged = Vec::new();
            for (idx, i) in layout.members(g).enumerate() {
                match read_slice(&file, file_len, &layout, i) {
                    Some((buf, len)) if crc32c::crc32c(&buf[..len]) == table.slice_crcs[i as usize] => {
                        originals.push((idx, buf))
                    }
                    _ => damaged.push((idx, i)),
                }
            }
            let abimees: Vec<u64> = damaged.iter().map(|d| d.1).collect();
            if damaged.is_empty() {
                return Ok((abimees, true, Vec::new()));
            }
            // Healthy recovery slices.
            let mut recovery = Vec::new();
            for r in 0..layout.recovery_count(g) {
                let n = starts[g as usize] + r;
                let offset = table.data_offset.saturating_add(n * layout.slice_size);
                // Nothing is reserved for a slice outside the file.
                if offset.saturating_add(layout.slice_size) > file_len {
                    continue;
                }
                let mut buf = vec![0u8; layout.slice_size as usize];
                if read_exact_at(&file, &mut buf, offset).is_ok() && crc32c::crc32c(&buf) == table.shard_crcs[n as usize] {
                    recovery.push((r as usize, buf));
                }
            }
            if damaged.len() > recovery.len() {
                return Ok((abimees, false, Vec::new()));
            }
            let k = layout.group_len(g) as usize;
            let restored = reed_solomon_simd::decode(k, layout.recovery_count(g) as usize, originals, recovery)
                .map_err(|e| anyhow::anyhow!("reconstruction impossible : {e}"))?;
            let mut fixed = Vec::new();
            for (idx, i) in &damaged {
                let data = restored.get(idx).context("tranche non reconstruite")?;
                let (_, len) = layout.slice_range(*i);
                if crc32c::crc32c(&data[..len]) != table.slice_crcs[*i as usize] {
                    bail!("tranche {i} reconstruite incorrecte");
                }
                fixed.push((*i, data[..len].to_vec()));
            }
            Ok((abimees, true, fixed))
        })
        .collect();

    let mut fixes = Vec::new();
    for r in results {
        let (abimees, reparable, fixed) = r?;
        let n = abimees.len() as u64;
        report.damaged += n;
        if reparable {
            report.repaired += n;
        } else {
            report.lost += n;
            report.lost_slices.extend_from_slice(&abimees);
        }
        report.damaged_slices.extend(abimees);
        fixes.extend(fixed);
    }
    report.damaged_slices.sort_unstable();
    report.lost_slices.sort_unstable();

    if let Some(out) = output {
        // The original is never touched: the repair goes into a copy.
        fs::copy(path, out).with_context(|| format!("impossible de créer {}", out.display()))?;
        let mut f = OpenOptions::new().write(true).open(out)?;
        for (i, data) in fixes {
            use std::io::{Seek, SeekFrom};
            f.seek(SeekFrom::Start(i * layout.slice_size))?;
            f.write_all(&data)?;
        }
        f.sync_all()?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoupage() {
        let l = Layout::new(10_000_000, 3);
        assert_eq!(l.slice_size, 4096);
        assert_eq!(l.slices(), 2442);
        assert_eq!(l.groups, 1);
        assert_eq!(l.recovery_count(0), 74);
        // Large archive: bigger slices, several groups.
        let l = Layout::new(50_000_000_000, 3);
        assert!(l.slices() <= MAX_SLICES);
        assert!(l.groups > 1);
        let members: u64 = (0..l.groups).map(|g| l.group_len(g)).sum();
        assert_eq!(members, l.slices());
    }

    #[test]
    fn garantie_par_blocs() {
        // 75 MB archive whose largest block is 12 MB: each group must be able to
        // rebuild at least the slices of a whole block.
        let l = Layout::for_blocks(75_000_000, 3, 1, 12_000_000);
        let per_group = (12_000_000u64.div_ceil(l.slice_size) + 1).div_ceil(l.groups);
        assert!((0..l.groups).all(|g| l.recovery_count(g) >= per_group));
        assert!(l.permille > 30);
    }

    #[test]
    fn table_aller_retour() {
        let layout = Layout::new(100_000, 5);
        let n = layout.slices() as usize;
        let r = *layout.recovery_starts().last().unwrap() as usize;
        let t = Table { layout, data_offset: 123, slice_crcs: (0..n as u32).collect(), shard_crcs: vec![7; r] };
        assert_eq!(Table::decode(&t.encode()).unwrap(), t);
    }
}
