//! amdgpu engine utilisation, from fdinfo.
//!
//! i915 publishes a system-wide busy counter per engine; amdgpu publishes no
//! PMU at all. What it does publish is each client's cumulative time per engine
//! class in `/proc/<pid>/fdinfo` (`drm-engine-gfx: 41857634272 ns`). The
//! advance of every client on the device over an interval, added up, is the
//! device's busy time per class: still a time ratio, not a sampled guess.
//!
//! Three things follow from that, and differ from i915:
//!
//! - A client's time runs from when its job reaches the hardware ring to when
//!   the job's fence signals, so a job queued behind another context's counts
//!   as busy. Several contexts on one class can therefore add up to more than
//!   wall time; the bar is clamped at 100 %.
//! - Work by a client that exits between two samples is lost, and a client that
//!   appears is measured from its first sighting, so short-lived clients are
//!   undercounted.
//! - Only processes the reader may inspect are counted. As root, that is all
//!   of them; otherwise only the user's own, and the display says so.
//!
//! The firmware's own `gpu_busy_percent` is shown alongside as a cross-check.

use crate::clients::Client;
use crate::pmu::{EngineDef, PmuSample};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

/// Engine classes as `(class, short name, fdinfo keys)`, in display order:
/// media first, as for i915. amdgpu prints an fdinfo key only once the class
/// has been used, so the set is fixed here rather than discovered. `enc_1` is
/// UVD's second encode ring; on VCN parts both encoders print as `enc`.
pub const ENGINES: [(&str, &str, &[&str]); 6] = [
    ("dec", "dec", &["dec"]),
    ("enc", "enc", &["enc", "enc_1"]),
    ("jpeg", "jpeg", &["jpeg"]),
    ("gfx", "gfx", &["gfx"]),
    ("dma", "dma", &["dma"]),
    ("compute", "comp", &["compute"]),
];

pub fn engine_defs() -> Vec<EngineDef> {
    ENGINES
        .iter()
        .map(|(class, name, _)| EngineDef { name: name.to_string(), class: class.to_string(), instance: 0 })
        .collect()
}

/// A client's cumulative nanoseconds per entry of `ENGINES`.
fn totals(c: &Client) -> [u64; ENGINES.len()] {
    let mut t = [0u64; ENGINES.len()];
    for (i, (_, _, keys)) in ENGINES.iter().enumerate() {
        t[i] = keys.iter().filter_map(|k| c.engines.get(*k)).sum();
    }
    t
}

/// The amdgpu cards present, as `(PCI address, /sys/class/drm/cardN)`.
pub fn discover() -> Vec<(String, PathBuf)> {
    crate::pmu::cards_on_driver("amdgpu")
        .into_iter()
        .filter_map(|pci| crate::pmu::card_for_pci(&pci).map(|card| (pci, card)))
        .collect()
}

pub struct EngineReader {
    /// The previous scan: when, and each client's totals by `drm-client-id`.
    last: Option<(Instant, HashMap<u64, [u64; ENGINES.len()]>)>,
}

impl EngineReader {
    pub fn new() -> Self {
        EngineReader { last: None }
    }

    pub fn reset(&mut self) {
        self.last = None;
    }

    /// Differentiates a scan of the device's clients against the previous one.
    /// The first call establishes a baseline and yields `None`.
    pub fn sample(&mut self, clients: &[Client]) -> Option<PmuSample> {
        let now = Instant::now();
        let current: HashMap<u64, [u64; ENGINES.len()]> =
            clients.iter().map(|c| (c.client_id, totals(c))).collect();
        let (t0, prev) = self.last.replace((now, current))?;
        let dt_ns = now.duration_since(t0).as_nanos() as f64;
        if dt_ns <= 0.0 {
            return None;
        }
        let current = &self.last.as_ref()?.1;
        Some(PmuSample {
            dt_s: dt_ns / 1e9,
            engine_busy: busy(&prev, current, dt_ns),
            ..Default::default()
        })
    }
}

/// Busy percentage per engine class: the summed advance of every client seen
/// in both scans, over the interval. A client's counters never run backwards,
/// so a smaller figure means a recycled client ID and is skipped.
fn busy(
    prev: &HashMap<u64, [u64; ENGINES.len()]>,
    cur: &HashMap<u64, [u64; ENGINES.len()]>,
    dt_ns: f64,
) -> Vec<f64> {
    let mut sum = [0u64; ENGINES.len()];
    for (id, now) in cur {
        if let Some(before) = prev.get(id) {
            for i in 0..ENGINES.len() {
                sum[i] += now[i].saturating_sub(before[i]);
            }
        }
    }
    sum.iter().map(|ns| (*ns as f64 / dt_ns * 100.0).clamp(0.0, 100.0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(id: u64, engines: &[(&str, u64)]) -> Client {
        Client {
            client_id: id,
            engines: engines.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn encoders_fold_into_one_class() {
        let t = totals(&client(1, &[("enc", 5), ("enc_1", 7), ("gfx", 3)]));
        assert_eq!(t, [0, 12, 0, 3, 0, 0]);
    }

    #[test]
    fn clients_add_and_newcomers_wait() {
        let prev: HashMap<_, _> =
            [(1, totals(&client(1, &[("gfx", 100)]))), (2, totals(&client(2, &[("dec", 0)])))].into();
        let cur: HashMap<_, _> = [
            (1, totals(&client(1, &[("gfx", 400)]))),
            (2, totals(&client(2, &[("dec", 250), ("gfx", 100)]))),
            // New since the last scan: its lifetime is not this interval's work.
            (3, totals(&client(3, &[("gfx", 900)]))),
        ]
        .into();
        let b = busy(&prev, &cur, 1000.0);
        assert_eq!(b, vec![25.0, 0.0, 0.0, 40.0, 0.0, 0.0]);
    }

    #[test]
    fn overlapping_contexts_clamp() {
        let prev: HashMap<_, _> = [(1, [0; 6]), (2, [0; 6])].into();
        let cur: HashMap<_, _> =
            [(1, totals(&client(1, &[("gfx", 800)]))), (2, totals(&client(2, &[("gfx", 700)])))].into();
        assert_eq!(busy(&prev, &cur, 1000.0)[3], 100.0);
    }
}
