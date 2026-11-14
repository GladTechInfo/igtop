//! i915 PMU discovery and sampling.
//!
//! Each i915 device publishes its own event source under
//! `/sys/bus/event_source/devices/`. A discrete card's is named
//! `i915_<pci address>`; an integrated GPU's is the bare `i915`. A machine with
//! both has both, and both shapes are handled. Event configs are read from sysfs
//! rather than hardcoded, so the same binary works across generations and does
//! not break when the encoding changes.

use crate::perf::PerfGroup;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const EVENT_SOURCES: &str = "/sys/bus/event_source/devices";

/// Engine classes, in the order they should be presented. Video first: igtop
/// was written for transcoding, so the media engines are the headline.
pub const ENGINE_ORDER: [&str; 5] = ["vcs", "vecs", "rcs", "bcs", "ccs"];

/// The classes that make up the MEDIA headline, on either driver.
pub const MEDIA_CLASSES: [&str; 5] = ["vcs", "vecs", "dec", "enc", "jpeg"];

pub fn engine_label(class: &str) -> &'static str {
    match class {
        "vcs" => "VIDEO",
        "vecs" => "ENHANCE",
        "rcs" => "RENDER",
        "bcs" => "COPY",
        "ccs" => "COMPUTE",
        // amdgpu classes; see `amdgpu::ENGINES`.
        "dec" => "DECODE",
        "enc" => "ENCODE",
        "jpeg" => "JPEG",
        "gfx" => "GFX",
        "dma" => "DMA",
        "compute" => "COMPUTE",
        _ => "OTHER",
    }
}

#[derive(Clone, Debug)]
pub struct EngineDef {
    pub name: String,
    pub class: String,
    pub instance: u32,
}

/// Splits `vcs1-busy` into class `vcs`, instance `1`.
fn parse_engine(event: &str) -> Option<EngineDef> {
    let stem = event.strip_suffix("-busy")?;
    let split = stem.find(|c: char| c.is_ascii_digit())?;
    let (class, inst) = stem.split_at(split);
    Some(EngineDef {
        name: stem.to_string(),
        class: class.to_string(),
        instance: inst.parse().ok()?,
    })
}

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// Event files hold `config=0x2000`, sometimes with further comma-separated
/// terms. Only the config term is meaningful for these counters.
fn parse_config(body: &str) -> Option<u64> {
    for term in body.split(',') {
        let term = term.trim();
        if let Some(v) = term.strip_prefix("config=") {
            let v = v.trim();
            return if let Some(hex) = v.strip_prefix("0x") {
                u64::from_str_radix(hex, 16).ok()
            } else {
                v.parse().ok()
            };
        }
    }
    None
}

#[derive(Clone, Debug)]
pub struct PmuDevice {
    pub pmu_name: String,
    pub pci_addr: Option<String>,
    pub pmu_type: u32,
    pub cpu: i32,
    pub engines: Vec<EngineDef>,
    pub events: BTreeMap<String, u64>,
}

/// `i915_DDDD_BB_DD.F` encodes the PCI address with underscores; the bare `i915`
/// name carries none, and is resolved against the card list by the caller.
fn pci_from_pmu_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("i915_")?;
    let parts: Vec<&str> = rest.split('_').collect();
    if parts.len() != 3 {
        return None;
    }
    Some(format!("{}:{}:{}", parts[0], parts[1], parts[2]))
}

pub fn discover() -> Vec<PmuDevice> {
    let mut out = Vec::new();
    let dir = match fs::read_dir(EVENT_SOURCES) {
        Ok(d) => d,
        Err(_) => return out,
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name != "i915" && !name.starts_with("i915_") {
            continue;
        }
        let base = entry.path();
        let pmu_type: u32 = match read_trim(&base.join("type")).and_then(|s| s.parse().ok()) {
            Some(t) => t,
            None => continue,
        };
        // cpumask names the single CPU the PMU's counters are readable from;
        // opening on any other CPU returns EINVAL.
        let cpu = read_trim(&base.join("cpumask"))
            .and_then(|s| s.split([',', '-']).next().and_then(|f| f.trim().parse().ok()))
            .unwrap_or(0);

        let mut events = BTreeMap::new();
        let mut engines = Vec::new();
        if let Ok(ev) = fs::read_dir(base.join("events")) {
            for e in ev.flatten() {
                let ename = e.file_name().to_string_lossy().to_string();
                // Skip the `.unit` / `.scale` sidecar files.
                if ename.contains('.') {
                    continue;
                }
                let body = match read_trim(&e.path()) {
                    Some(b) => b,
                    None => continue,
                };
                if let Some(cfg) = parse_config(&body) {
                    if let Some(def) = parse_engine(&ename) {
                        engines.push(def);
                    }
                    events.insert(ename, cfg);
                }
            }
        }
        engines.sort_by(|a, b| {
            let ai = ENGINE_ORDER.iter().position(|c| *c == a.class).unwrap_or(99);
            let bi = ENGINE_ORDER.iter().position(|c| *c == b.class).unwrap_or(99);
            ai.cmp(&bi).then(a.instance.cmp(&b.instance))
        });

        out.push(PmuDevice {
            pci_addr: pci_from_pmu_name(&name),
            pmu_name: name,
            pmu_type,
            cpu,
            engines,
            events,
        });
    }
    // Deterministic order, and the discrete card ahead of the bare `i915` iGPU.
    out.sort_by(|a, b| b.pmu_name.len().cmp(&a.pmu_name.len()).then(a.pmu_name.cmp(&b.pmu_name)));
    out
}

/// Non-engine counters worth sampling, in fixed order.
const AUX_EVENTS: [&str; 5] = [
    "actual-frequency",
    "requested-frequency",
    "rc6-residency",
    "interrupts",
    "software-gt-awake-time",
];

pub struct PmuReader {
    group: PerfGroup,
    /// Index into a group read for each engine, parallel to `engines`.
    engine_slots: Vec<usize>,
    aux_slots: [Option<usize>; 5],
    pub engines: Vec<EngineDef>,
    /// The previous read: the kernel's `time_enabled` and the counter values.
    last: Option<(u64, Vec<u64>)>,
}

/// One interval's worth of derived figures. Every field is a rate or a
/// percentage; nothing here is a raw accumulator.
#[derive(Clone, Debug, Default)]
pub struct PmuSample {
    /// Length of the interval this sample covers, in seconds, by the kernel's
    /// clock.
    pub dt_s: f64,
    /// Engine busy percentage, parallel to `PmuReader::engines`.
    pub engine_busy: Vec<f64>,
    /// Mean clock across the whole interval, sleep included. The kernel
    /// integrates 0 MHz while the GT is parked, so on a bursty load this reads
    /// the running clock multiplied by the awake fraction — 2000 MHz for 30% of
    /// a second shows as ~600. Kept because it is what intel_gpu_top reports.
    pub actual_freq_avg_mhz: f64,
    pub requested_freq_avg_mhz: f64,
    /// Mean clock while the GT was awake: the frequency integral divided by
    /// awake time instead of wall time. This is the "what clock did it run at"
    /// figure. Zero when the GT slept through the interval.
    pub actual_freq_mhz: f64,
    pub requested_freq_mhz: f64,
    pub rc6_pct: f64,
    pub interrupts_per_s: f64,
    pub awake_pct: f64,
}

impl PmuReader {
    pub fn open(dev: &PmuDevice) -> std::io::Result<Self> {
        let mut group = PerfGroup::new();
        let mut engine_slots = Vec::new();
        let mut engines = Vec::new();

        for eng in &dev.engines {
            let key = format!("{}-busy", eng.name);
            if let Some(&cfg) = dev.events.get(&key) {
                if group.add(dev.pmu_type, cfg, dev.cpu).is_ok() {
                    engine_slots.push(group.len() - 1);
                    engines.push(eng.clone());
                }
            }
        }

        let mut aux_slots: [Option<usize>; 5] = [None; 5];
        for (i, key) in AUX_EVENTS.iter().enumerate() {
            if let Some(&cfg) = dev.events.get(*key) {
                if group.add(dev.pmu_type, cfg, dev.cpu).is_ok() {
                    aux_slots[i] = Some(group.len() - 1);
                }
            }
        }

        if group.len() == 0 {
            return Err(std::io::Error::other(
                "no i915 PMU counters could be opened (run as root: perf_event_paranoid is 4)",
            ));
        }

        Ok(PmuReader { group, engine_slots, aux_slots, engines, last: None })
    }

    /// Samples the group and differentiates against the previous call. The first
    /// call establishes a baseline and yields `None`, since a rate needs two points.
    pub fn sample(&mut self) -> std::io::Result<Option<PmuSample>> {
        let (enabled, vals) = self.group.read()?;

        let prev = match self.last.replace((enabled, vals.clone())) {
            Some(p) => p,
            None => return Ok(None),
        };
        // The interval is measured by the kernel, in the same read as the
        // counters — see `PerfGroup::read`.
        let dt_ns = enabled.saturating_sub(prev.0) as f64;
        if dt_ns <= 0.0 {
            return Ok(None);
        }
        let dt = dt_ns / 1e9;
        let d = |slot: usize| -> f64 { vals[slot].saturating_sub(prev.1[slot]) as f64 };

        // Engine counters accumulate busy nanoseconds, so the busy fraction is a
        // straight time ratio against the wall interval.
        let engine_busy = self
            .engine_slots
            .iter()
            .map(|&s| (d(s) / dt_ns * 100.0).clamp(0.0, 100.0))
            .collect();

        // Frequency counters accumulate MHz-seconds, so the delta over the
        // interval is the mean frequency across it. The kernel only integrates
        // while the GT is unparked ("Report 0/0 (actual/requested) frequency
        // while parked", i915_pmu.c), so the wall-time mean is diluted by sleep.
        let freq_avg = |slot: Option<usize>| -> f64 { slot.map(|s| d(s) / dt).unwrap_or(0.0) };

        // Dividing the same integral by awake time recovers the clock the GT
        // actually ran at. The kernel samples frequency on a 5 ms timer, so an
        // interval with less awake time than one period has no usable integral
        // and reads 0. Without the awake counter, fall back to the wall mean.
        let awake_s = self.aux_slots[4].map(|s| d(s) / 1e9);
        let freq_awake = |slot: Option<usize>| -> f64 {
            match (slot, awake_s) {
                (Some(s), Some(a)) if a >= 0.005 => d(s) / a,
                (Some(_), Some(_)) => 0.0,
                (s, None) => freq_avg(s),
                (None, _) => 0.0,
            }
        };

        Ok(Some(PmuSample {
            dt_s: dt,
            engine_busy,
            actual_freq_avg_mhz: freq_avg(self.aux_slots[0]),
            requested_freq_avg_mhz: freq_avg(self.aux_slots[1]),
            actual_freq_mhz: freq_awake(self.aux_slots[0]),
            requested_freq_mhz: freq_awake(self.aux_slots[1]),
            rc6_pct: self.aux_slots[2].map(|s| (d(s) / dt_ns * 100.0).clamp(0.0, 100.0)).unwrap_or(0.0),
            interrupts_per_s: self.aux_slots[3].map(|s| d(s) / dt).unwrap_or(0.0),
            awake_pct: self.aux_slots[4].map(|s| (d(s) / dt_ns * 100.0).clamp(0.0, 100.0)).unwrap_or(0.0),
        }))
    }

    /// Drops the baseline, so the next `sample` starts a fresh interval. Used
    /// after a pause: otherwise the first sample would average the whole pause
    /// and present it as the current reading.
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// A reader with engines but no counters, for rendering tests.
    #[cfg(test)]
    pub fn fake(engines: Vec<EngineDef>) -> Self {
        let engine_slots = (0..engines.len()).collect();
        PmuReader { group: PerfGroup::new(), engine_slots, aux_slots: [None; 5], engines, last: None }
    }
}

/// PCI addresses of the DRM cards bound to `driver`, sorted.
pub fn cards_on_driver(driver: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(dir) = fs::read_dir("/sys/class/drm") {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let dev = e.path().join("device");
            let bound = fs::canonicalize(dev.join("driver"))
                .ok()
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy() == driver));
            if bound == Some(true) {
                if let Some(addr) = fs::canonicalize(&dev).ok().and_then(|p| {
                    p.file_name().map(|f| f.to_string_lossy().to_string())
                }) {
                    out.push(addr);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Maps a PCI address to its `/sys/class/drm/cardN` directory.
pub fn card_for_pci(pci: &str) -> Option<PathBuf> {
    let dir = fs::read_dir("/sys/class/drm").ok()?;
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let dev = e.path().join("device");
        if let Ok(real) = fs::canonicalize(&dev) {
            if real.file_name().map(|f| f.to_string_lossy() == pci).unwrap_or(false) {
                return Some(e.path());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_events() {
        let e = parse_engine("vecs1-busy").unwrap();
        assert_eq!((e.name.as_str(), e.class.as_str(), e.instance), ("vecs1", "vecs", 1));
        assert!(parse_engine("vcs0-wait").is_none());
        assert!(parse_engine("actual-frequency").is_none());
    }

    #[test]
    fn configs() {
        assert_eq!(parse_config("config=0x2000"), Some(0x2000));
        assert_eq!(parse_config("config=0x100002,other=1"), Some(0x100002));
        assert_eq!(parse_config("config=42"), Some(42));
        assert_eq!(parse_config("event=0x1"), None);
    }

    #[test]
    fn pmu_names() {
        assert_eq!(pci_from_pmu_name("i915_0000_03_00.0").as_deref(), Some("0000:03:00.0"));
        assert_eq!(pci_from_pmu_name("i915"), None);
    }
}
