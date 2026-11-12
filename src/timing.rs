//! Timing of the steps: wall time, CPU time of all cores, and therefore the
//! number of cores actually working (--chrono option).

use std::fs;
use std::time::Instant;

/// Duration of a step: wall time and cumulated CPU time of all cores.
pub struct Step {
    pub name: &'static str,
    pub wall: f64,
    pub cpu: f64,
}

pub(crate) struct Chrono {
    start: Instant,
    cpu: f64,
    pub(crate) steps: Vec<Step>,
}

impl Chrono {
    pub(crate) fn new() -> Self {
        Self { start: Instant::now(), cpu: cpu_seconds(), steps: Vec::new() }
    }

    pub(crate) fn lap(&mut self, name: &'static str) {
        let cpu = cpu_seconds();
        self.steps.push(Step { name, wall: self.start.elapsed().as_secs_f64(), cpu: cpu - self.cpu });
        self.start = Instant::now();
        self.cpu = cpu;
    }
}

/// CPU time consumed by the process (all cores), read from /proc.
fn cpu_seconds() -> f64 {
    let Ok(stat) = fs::read_to_string("/proc/self/stat") else { return 0.0 };
    let fields: Vec<&str> = stat.rsplit(')').next().unwrap_or("").split_whitespace().collect();
    let ticks: u64 = fields.get(11..13).map_or(0, |f| f.iter().filter_map(|v| v.parse::<u64>().ok()).sum());
    // 100 ticks per second: standard Linux kernel value.
    ticks as f64 / 100.0
}

