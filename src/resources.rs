//! Settings according to the machine: block size and number of working
//! cores, from the amount of data, the number of cores and the free memory.
//! CNz thus adapts as well to a server as to a Raspberry Pi.

/// Bounds of the automatic block size. Below 8 MB compression clearly
/// degrades; beyond 32 MB it gains almost nothing more (zstd window) and
/// keeps fewer cores busy.
pub const MIN_AUTO_BLOCK: u64 = 8 << 20;
pub const MAX_AUTO_BLOCK: u64 = 32 << 20;
/// Memory of a core compressing a block, in number of blocks: the block, the
/// compressed result and the zstd tables (measured: a little over 4).
const MEM_PER_BLOCK: u64 = 5;
/// Base outside blocks: the program, the file list, the index (measured:
/// about 60 MB for 30,000 files).
const BASE: u64 = 64 << 20;
const MO: u64 = 1 << 20;
/// Never less than a 64 MB budget: below that, nothing would work.
const MIN_BUDGET: u64 = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    pub block_size: u64,
    pub workers: usize,
    pub budget: u64,
}

pub fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Memory available on the machine, in bytes.
pub fn available_memory() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.available_memory()
}

/// Memory budget: the requested limit, otherwise half of the free memory.
pub fn budget(max_memory: Option<u64>) -> u64 {
    max_memory.unwrap_or_else(|| available_memory() / 2).max(MIN_BUDGET)
}

/// Block size and number of cores to compress `total` bytes; `maximum`:
/// context-mixing engine (its extra memory per core).
pub fn plan(total: u64, user_block: Option<u64>, max_memory: Option<u64>, maximum: bool) -> Plan {
    plan_for(total, user_block, budget(max_memory), cores(), maximum)
}

/// Memory of a core, on top of the block, for the context-mixing engine.
fn cm_memory(block: u64, maximum: bool) -> u64 {
    if maximum {
        crate::cm::MEMORY + block
    } else {
        0
    }
}

fn plan_for(total: u64, user_block: Option<u64>, budget: u64, cores: usize, maximum: bool) -> Plan {
    let usable = budget.saturating_sub(BASE).max(16 * MO);
    let mut block = user_block.unwrap_or_else(|| {
        if total.div_ceil(MAX_AUTO_BLOCK) >= cores as u64 {
            // 32 MB blocks are enough to keep all the cores busy.
            MAX_AUTO_BLOCK
        } else {
            // Otherwise, smaller blocks so that each core gets one, without going
            // below 8 MB.
            total.div_ceil(cores as u64).next_multiple_of(MO).clamp(MIN_AUTO_BLOCK, MAX_AUTO_BLOCK)
        }
    });
    // If even a single core does not fit in the budget, the blocks are reduced
    // (unless the user imposed their size).
    if user_block.is_none() && MEM_PER_BLOCK * block + cm_memory(block, maximum) > usable {
        let per_mo = MEM_PER_BLOCK + u64::from(maximum);
        block = (usable.saturating_sub(cm_memory(0, maximum)) / per_mo / MO).max(1) * MO;
    }
    let workers = ((usable / (MEM_PER_BLOCK * block + cm_memory(block, maximum))) as usize).clamp(1, cores);
    Plan { block_size: block, workers, budget }
}

/// Number of blocks to decompress at the same time during extraction: each
/// block takes its compressed size and its decompressed size.
/// `cm`: the archive has context-mixing engine blocks.
pub fn extract_batch(largest_raw: u64, cm: bool, max_memory: Option<u64>) -> usize {
    let per_block = 2 * largest_raw.max(1) + cm_memory(largest_raw, cm);
    ((budget(max_memory).saturating_sub(BASE) / per_block) as usize).clamp(1, 2 * cores())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GO: u64 = 1 << 30;

    #[test]
    fn blocs_selon_le_volume() {
        // 240 MB on 8 cores: 8 blocks of 32 MB already keep all the cores busy.
        assert_eq!(plan_for(240 * MO, None, 8 * GO, 8, false).block_size, 32 * MO);
        // 100 MB: 4 blocks of 32 MB would leave 4 cores idle.
        assert_eq!(plan_for(100 * MO, None, 8 * GO, 8, false).block_size, 13 * MO);
        // Small folder: 8 MB minimum.
        assert_eq!(plan_for(56 * MO, None, 8 * GO, 8, false).block_size, 8 * MO);
        // Huge volume: 32 MB maximum, all cores.
        assert_eq!(plan_for(50 * GO, None, 8 * GO, 8, false), Plan { block_size: 32 * MO, workers: 8, budget: 8 * GO });
    }

    #[test]
    fn peu_de_memoire() {
        // 500 MB budget: 436 MB outside the base, 160 MB per core, so 2 cores.
        assert_eq!(plan_for(50 * GO, None, 500 * MO, 8, false).workers, 2);
        // 100 MB budget: the blocks shrink so that one core fits.
        let p = plan_for(50 * GO, None, 100 * MO, 8, false);
        assert_eq!((p.block_size, p.workers), (7 * MO, 1));
        // Size imposed by the user: respected, a single core.
        assert_eq!(plan_for(50 * GO, Some(32 * MO), 64 * MO, 8, false).block_size, 32 * MO);
    }

    #[test]
    fn mode_maximum() {
        // 1 GB budget: 960 MB outside the base; a core counts 5 × 32 MB of blocks,
        // plus 96 MB of tables and 32 MB of engine data.
        let p = plan_for(50 * GO, None, GO, 8, true);
        assert_eq!((p.block_size, p.workers), (32 * MO, 3));
        assert_eq!(plan_for(50 * GO, None, GO, 8, false).workers, 6);
        // Very little memory: the blocks shrink, one core.
        let p = plan_for(50 * GO, None, 200 * MO, 8, true);
        assert_eq!(p.workers, 1);
        assert!(p.block_size >= MO && 6 * p.block_size + crate::cm::MEMORY <= 136 * MO);
    }
}
