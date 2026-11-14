//! Per-GPU state: joins the engine, sysfs and client sources and keeps history.
//!
//! A `Gpu` is driver-neutral above its `Source`, so i915 and amdgpu cards sit
//! side by side in one list and one display.

use crate::amdgpu;
use crate::clients::{self, ClientRate, ClientTracker};
use crate::pmu::{self, EngineDef, PmuDevice, PmuReader, PmuSample};
use crate::sysfs;
use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;

/// History columns, one per second of sampled time: ten minutes, whatever the
/// sampling rate. It used to be one per sample, so at `-i 50` the same buffer
/// held thirty seconds, and changing the rate mid-run mixed time scales on one
/// unlabelled axis.
pub const HISTORY: usize = 600;

/// Asymmetric damping, in the manner of a shock absorber: quick to compress,
/// slow to rebound. A rising edge reaches the bar almost at once so a spike is
/// never missed, whilst the fall is damped so the bar settles rather than
/// flickering. Symmetric smoothing would blunt the spikes too, and the spikes
/// are the one thing worth seeing.
///
/// These are **time constants in seconds**, not fixed per-sample weights. A
/// fixed weight would tie the feel to the refresh rate — the same setting would
/// decay four times more slowly at `-i 250` than at the 1 s default. Converted
/// per tick as `alpha = 1 - exp(-dt / tau)`.
///
/// The rise is this many times quicker than the fall. Derived from the decay
/// rather than set independently, so one knob (`--damping`) moves the whole
/// feel and the asymmetry is preserved at any setting.
const ATTACK_RATIO: f64 = 3.0;
pub const DEFAULT_DAMPING_S: f64 = 4.0;

/// Percentage points below which a bar counts as having arrived. A tenth of a
/// point is well under one eighth-cell on any realistic pane width, so nothing
/// visible is lost by stopping there.
const SETTLED: f64 = 0.1;

/// Seconds-to-90% converted to the time constant the filter actually uses.
/// Non-finite input (`--damping nan`) is rejected at argument parsing; the
/// clamp here only bounds finite values.
pub fn damping_tau(seconds: f64) -> f64 {
    (seconds.clamp(0.2, 30.0)) / std::f64::consts::LN_10
}

/// The capacity gauges: TEMP, POWER, CLOCK, MEMORY.
pub const CAPACITY: usize = 4;

/// One frame of the shock-absorber filter: moves `prev` toward `target` with
/// the attack constant when rising and the decay constant when falling.
/// Returns whether it is still visibly travelling; within `SETTLED` the gap is
/// closed outright, so a settled bar reads exactly the sampled figure.
fn glide(prev: &mut f64, target: f64, dt: f64, attack: f64, decay: f64) -> bool {
    let tau = if target > *prev { attack } else { decay };
    *prev += (target - *prev) * (1.0 - (-dt / tau).exp());
    if (target - *prev).abs() < SETTLED {
        *prev = target;
        false
    } else {
        true
    }
}

fn push(rings: &mut [VecDeque<u64>], i: usize, value: f64, times: usize) {
    if let Some(h) = rings.get_mut(i) {
        for _ in 0..times {
            if h.len() == HISTORY {
                h.pop_front();
            }
            h.push_back(value.round().clamp(0.0, 100.0) as u64);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Driver {
    I915,
    Amdgpu,
}

impl Driver {
    pub fn name(self) -> &'static str {
        match self {
            Driver::I915 => "i915",
            Driver::Amdgpu => "amdgpu",
        }
    }
}

/// Where the engine figures come from.
pub enum Source {
    /// i915's PMU: system-wide busy counters per engine.
    I915(PmuReader),
    /// amdgpu has no PMU; engines are summed from every client's fdinfo.
    Amd(amdgpu::EngineReader),
}

pub struct Gpu {
    pub pmu_name: String,
    pub pci_addr: Option<String>,
    pub driver: Driver,
    source: Source,
    /// The engines shown, in display order.
    pub engines: Vec<EngineDef>,
    /// amdgpu figures cover only processes this one may inspect: all of them
    /// as root, the user's own otherwise.
    pub partial: bool,
    pub sysfs: Option<sysfs::Reader>,
    pub sample: PmuSample,
    pub dynamic: sysfs::Dynamic,
    /// One ring per engine, parallel to `engines`, holding busy percent.
    /// Damped copy of `sample.engine_busy`, for display only. The raw values
    /// remain untouched, so `--once` and the history sparkline stay honest.
    pub smooth: Vec<f64>,
    smooth_primed: bool,
    /// Damped copies of `capacity_pct`, for the TEMP/POWER/CLOCK/MEMORY bars.
    cap_smooth: [f64; CAPACITY],
    cap_primed: [bool; CAPACITY],
    decay_tau: f64,
    pub history: Vec<VecDeque<u64>>,
    /// Damped trace, parallel to `history`. Kept alongside the raw one rather
    /// than replacing it: the graph should agree with the bars above it, but
    /// `--once` and any measurement must still see untouched values.
    pub history_smooth: Vec<VecDeque<u64>>,
    /// Busy-seconds per engine, and sampled seconds, not yet in a history
    /// column.
    hist_acc: Vec<f64>,
    hist_acc_s: f64,
    pub clients: Vec<ClientRate>,
    tracker: ClientTracker,
    /// The last PMU read failed, so every PMU figure on screen is the last good
    /// one. Shown, rather than leaving a frozen display to pass for a live one.
    pub pmu_failed: bool,
}

impl Gpu {
    fn with_source(
        pmu_name: String,
        pci_addr: Option<String>,
        driver: Driver,
        source: Source,
        engines: Vec<EngineDef>,
        card: Option<PathBuf>,
    ) -> Self {
        let n = engines.len();
        Gpu {
            pmu_name,
            pci_addr,
            driver,
            source,
            engines,
            partial: false,
            sysfs: card.map(sysfs::Reader::new),
            sample: PmuSample { engine_busy: vec![0.0; n], ..Default::default() },
            smooth: vec![0.0; n],
            smooth_primed: false,
            cap_smooth: [0.0; CAPACITY],
            cap_primed: [false; CAPACITY],
            decay_tau: damping_tau(DEFAULT_DAMPING_S),
            dynamic: sysfs::Dynamic::default(),
            history: vec![VecDeque::with_capacity(HISTORY); n],
            history_smooth: vec![VecDeque::with_capacity(HISTORY); n],
            hist_acc: vec![0.0; n],
            hist_acc_s: 0.0,
            clients: Vec::new(),
            tracker: ClientTracker::new(),
            pmu_failed: false,
        }
    }

    pub fn new(dev: &PmuDevice, card: Option<PathBuf>) -> std::io::Result<Self> {
        let reader = PmuReader::open(dev)?;
        let engines = reader.engines.clone();
        Ok(Self::with_source(
            dev.pmu_name.clone(),
            dev.pci_addr.clone(),
            Driver::I915,
            Source::I915(reader),
            engines,
            card,
        ))
    }

    /// An amdgpu card. Nothing here needs privilege to open, so this cannot
    /// fail; without root the figures are partial, and `partial` says so.
    pub fn new_amd(pci: &str, card: PathBuf) -> Self {
        let mut g = Self::with_source(
            "amdgpu".into(),
            Some(pci.to_string()),
            Driver::Amdgpu,
            Source::Amd(amdgpu::EngineReader::new()),
            amdgpu::engine_defs(),
            Some(card),
        );
        g.partial = unsafe { libc::geteuid() } != 0;
        g
    }

    /// A GPU with engines but no counters or sysfs, for rendering tests.
    #[cfg(test)]
    pub fn fake(engines: Vec<pmu::EngineDef>) -> Self {
        let mut g = Self::with_source(
            "i915_test".into(),
            Some("0000:03:00.0".into()),
            Driver::I915,
            Source::I915(PmuReader::fake(engines.clone())),
            engines,
            None,
        );
        g.smooth_primed = true;
        g
    }

    pub fn name(&self) -> String {
        self.sysfs
            .as_ref()
            .map(|s| s.stat.name.clone())
            .unwrap_or_else(|| self.pmu_name.clone())
    }

    /// rc6 residency: the hardware counter via sysfs where there is one, the
    /// PMU otherwise. See `sysfs::Reader::sample` for why the PMU is not
    /// trusted for this.
    pub fn rc6_pct(&self) -> f64 {
        self.dynamic.rc6_pct.unwrap_or(self.sample.rc6_pct)
    }

    /// TEMP, POWER, CLOCK and MEMORY as percentages of their ceilings, raw, in
    /// that order; `None` where the card has no such reading or no ceiling.
    pub fn capacity_pct(&self) -> [Option<f64>; CAPACITY] {
        let st = match self.sysfs.as_ref() {
            Some(s) => &s.stat,
            None => return [None; CAPACITY],
        };
        let d = &self.dynamic;
        let pct = |v: f64, max: f64| (v / max * 100.0).clamp(0.0, 100.0);
        [
            d.temp_c.map(|t| pct(t, st.temp_max_c)),
            (st.power_max_w > 0.0).then(|| pct(d.power_w.unwrap_or(0.0), st.power_max_w)),
            (st.freq_max_mhz > 0).then(|| pct(self.sample.actual_freq_mhz, st.freq_max_mhz as f64)),
            (st.vram_total_bytes > 0)
                .then(|| pct(d.vram_used_bytes.unwrap_or(0) as f64, st.vram_total_bytes as f64)),
        ]
    }

    /// Displayed capacity gauges: damped, or raw when smoothing is off.
    pub fn capacity_values(&self, smooth: bool) -> [Option<f64>; CAPACITY] {
        let raw = self.capacity_pct();
        if !smooth {
            return raw;
        }
        let mut out = raw;
        for i in 0..CAPACITY {
            if raw[i].is_some() && self.cap_primed[i] {
                out[i] = Some(self.cap_smooth[i]);
            }
        }
        out
    }

    /// Displayed engine values: damped, or raw when smoothing is off.
    pub fn engine_values(&self, smooth: bool) -> &[f64] {
        if smooth {
            &self.smooth
        } else {
            &self.sample.engine_busy
        }
    }

    /// The headline figure for a transcoding box: the busiest media engine.
    pub fn media_busy(&self, smooth: bool) -> f64 {
        self.engines
            .iter()
            .zip(self.engine_values(smooth).iter())
            .filter(|(e, _)| pmu::MEDIA_CLASSES.contains(&e.class.as_str()))
            .map(|(_, v)| *v)
            .fold(0.0_f64, f64::max)
    }

    pub fn set_damping(&mut self, seconds: f64) {
        self.decay_tau = damping_tau(seconds);
    }

    /// Which trace the graph should draw, matching the `s` toggle.
    pub fn history_of(&self, i: usize, smooth: bool) -> Option<&VecDeque<u64>> {
        if smooth {
            self.history_smooth.get(i)
        } else {
            self.history.get(i)
        }
    }

    /// Advances the damped values toward the latest sample. Called every frame,
    /// not every sample.
    ///
    /// This is what makes the motion continuous. The PMU is read once a second,
    /// so a filter applied at sample time can only ever step the bar once a
    /// second however gentle its constants are. Running the same filter at frame
    /// rate against the standing target lets the bar travel between samples —
    /// the difference between a stepper and a shock absorber.
    ///
    /// Returns whether any bar is still visibly travelling. Once every bar is
    /// within `SETTLED` of its target the frame would be identical to the last,
    /// so the caller stops redrawing until something changes. The remaining gap
    /// is closed outright, so a settled bar reads exactly the sampled figure.
    pub fn animate(&mut self, dt_frame: f64) -> bool {
        if !self.smooth_primed {
            return false;
        }
        let dt = dt_frame.clamp(0.001, 1.0);
        let decay = self.decay_tau;
        let attack = decay / ATTACK_RATIO;
        let mut moving = false;
        for (i, prev) in self.smooth.iter_mut().enumerate() {
            let target = self.sample.engine_busy.get(i).copied().unwrap_or(0.0);
            moving |= glide(prev, target, dt, attack, decay);
        }
        // The capacity gauges ride the same filter, so TEMP, POWER, CLOCK and
        // MEMORY settle the way the engine bars do instead of dropping the
        // instant the GPU goes idle. Their figures stay raw.
        let targets = self.capacity_pct();
        for (prev, target) in self.cap_smooth.iter_mut().zip(targets) {
            if let Some(t) = target {
                moving |= glide(prev, t, dt, attack, decay);
            }
        }
        moving
    }

    /// Puts every damped value on its target at once. Used while damping is
    /// off (`s`, `--no-smooth`): nothing glides then, so nothing should keep the
    /// screen redrawing, and turning damping back on starts from where the bars
    /// actually are rather than from wherever they were left.
    pub fn snap_smooth(&mut self) {
        for (prev, v) in self.smooth.iter_mut().zip(self.sample.engine_busy.iter()) {
            *prev = *v;
        }
        let targets = self.capacity_pct();
        for (prev, t) in self.cap_smooth.iter_mut().zip(targets) {
            if let Some(t) = t {
                *prev = t;
            }
        }
    }

    /// Discards every rate baseline — PMU, energy and clients — so the first
    /// readings after a pause cover a fresh interval rather than the pause.
    pub fn reset_rates(&mut self) {
        match &mut self.source {
            Source::I915(r) => r.reset(),
            Source::Amd(r) => r.reset(),
        }
        self.hist_acc.iter_mut().for_each(|a| *a = 0.0);
        self.hist_acc_s = 0.0;
        if let Some(sf) = self.sysfs.as_mut() {
            sf.reset();
        }
        self.tracker.reset();
    }

    /// As `reset_rates`, for the client table alone: used when it is shown
    /// again after being hidden, since hidden means not scanned. Rescans at
    /// once, so the table lists the current clients (at zero for one interval)
    /// rather than stale rows or none.
    pub fn reset_clients(&mut self) {
        self.tracker.reset();
        self.clients = self.tracker.update(self.pci_addr.as_deref());
    }

    /// Adds a sample to the pending history column and emits whole columns.
    ///
    /// Each column is the time-weighted mean over one second of sampled time.
    /// A sample spanning several seconds (`-i 2000`) fills that many columns, so
    /// one column is always one second. Rounding gets a twentieth of a second's
    /// grace, so `-i 250` closes a column on its fourth sample however the
    /// kernel's intervals jitter; any overshoot carries into the next column at
    /// the same mean rather than being dropped.
    fn record_history(&mut self, s: &PmuSample) {
        for (a, v) in self.hist_acc.iter_mut().zip(s.engine_busy.iter()) {
            *a += v * s.dt_s;
        }
        self.hist_acc_s += s.dt_s;
        let cols = ((self.hist_acc_s + 0.05).floor() as usize).min(HISTORY);
        if cols == 0 {
            return;
        }
        let rem = (self.hist_acc_s - cols as f64).max(0.0);
        for i in 0..self.hist_acc.len() {
            let mean = self.hist_acc[i] / self.hist_acc_s;
            push(&mut self.history, i, mean, cols);
            // The damped trace takes the bar's position at the moment the
            // column closes, so the graph agrees with the bars above it.
            let damped = self.smooth.get(i).copied().unwrap_or(mean);
            push(&mut self.history_smooth, i, damped, cols);
            self.hist_acc[i] = mean * rem;
        }
        self.hist_acc_s = rem;
    }

    /// Records the current `sample` into history, as a real tick would.
    #[cfg(test)]
    pub fn tick_fake(&mut self) {
        let s = self.sample.clone();
        self.record_history(&s);
    }

    pub fn tick(&mut self, scan_clients: bool) {
        // amdgpu's engines come from the client scan itself, so it is made on
        // every tick, and the table, when due, is fed from the same scan.
        let mut scanned = None;
        let result = match &mut self.source {
            Source::I915(r) => r.sample(),
            Source::Amd(r) => {
                let list = clients::scan(self.pci_addr.as_deref());
                let s = r.sample(&list);
                scanned = Some(list);
                Ok(s)
            }
        };
        match result {
            Ok(Some(s)) => {
                // The first real reading is adopted outright; ramping up from
                // zero would render as a spike that never happened.
                if !self.smooth_primed {
                    for (prev, v) in self.smooth.iter_mut().zip(s.engine_busy.iter()) {
                        *prev = *v;
                    }
                    self.smooth_primed = true;
                }
                self.record_history(&s);
                self.sample = s;
                self.pmu_failed = false;
            }
            Ok(None) => self.pmu_failed = false,
            Err(_) => self.pmu_failed = true,
        }

        if let Some(sf) = self.sysfs.as_mut() {
            self.dynamic = sf.sample();
        }

        // amdgpu's clock is the achieved shader clock at this instant; there is
        // no integral to divide, so the averaged figures are the same reading.
        if self.driver == Driver::Amdgpu {
            let mhz = self.dynamic.sclk_mhz.unwrap_or(0.0);
            self.sample.actual_freq_mhz = mhz;
            self.sample.actual_freq_avg_mhz = mhz;
        }

        // As with the engines, each gauge's first real reading is adopted
        // outright rather than ramped up to from zero. Power needs two energy
        // readings, so it arrives a tick after the rest.
        let targets = self.capacity_pct();
        for ((prev, primed), target) in self.cap_smooth.iter_mut().zip(self.cap_primed.iter_mut()).zip(targets) {
            if let (false, Some(t)) = (*primed, target) {
                *prev = t;
                *primed = true;
            }
        }

        // Gated to ~1 Hz by the caller regardless of refresh rate: fdinfo
        // counters move slowly and nothing is gained by reading them faster.
        if scan_clients {
            self.clients = match scanned {
                Some(list) => self.tracker.update_with(list),
                None => self.tracker.update(self.pci_addr.as_deref()),
            };
        }
    }
}

/// Pairs each PMU with its DRM card.
///
/// A PMU named `i915_DDDD_BB_DD.F` resolves directly. The bare `i915` PMU
/// carries no address, so it is matched to whichever i915 card no other PMU
/// claimed — in practice the integrated GPU.
pub fn resolve(devices: &[PmuDevice]) -> Vec<(PmuDevice, Option<PathBuf>)> {
    let mut all_cards: Vec<(PathBuf, String)> = Vec::new();
    if let Ok(dir) = fs::read_dir("/sys/class/drm") {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let driver = fs::canonicalize(e.path().join("device/driver"))
                .ok()
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()));
            if driver.as_deref() != Some("i915") {
                continue;
            }
            if let Ok(real) = fs::canonicalize(e.path().join("device")) {
                if let Some(addr) = real.file_name() {
                    all_cards.push((e.path(), addr.to_string_lossy().to_string()));
                }
            }
        }
    }
    all_cards.sort();

    let mut claimed: Vec<String> = Vec::new();
    let mut out = Vec::new();

    for d in devices {
        let card = match &d.pci_addr {
            Some(addr) => {
                claimed.push(addr.clone());
                pmu::card_for_pci(addr)
            }
            None => None,
        };
        out.push((d.clone(), card));
    }

    for (dev, card) in out.iter_mut() {
        if card.is_none() && dev.pci_addr.is_none() {
            if let Some((p, addr)) = all_cards.iter().find(|(_, a)| !claimed.contains(a)) {
                dev.pci_addr = Some(addr.clone());
                claimed.push(addr.clone());
                *card = Some(p.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the filter at 60 fps for `secs` from `from` toward `to`.
    fn run(from: f64, to: f64, secs: f64) -> f64 {
        let decay = damping_tau(DEFAULT_DAMPING_S);
        let mut v = from;
        for _ in 0..(secs * 60.0) as usize {
            glide(&mut v, to, 1.0 / 60.0, decay / ATTACK_RATIO, decay);
        }
        v
    }

    #[test]
    fn falls_slowly_rises_fast() {
        // --damping is seconds for a fall to be ~90% complete.
        let fell = run(100.0, 0.0, DEFAULT_DAMPING_S);
        assert!((9.0..11.0).contains(&fell), "after {} s a fall from 100 is at {}", DEFAULT_DAMPING_S, fell);
        // Half-way through, a falling bar is still well up.
        assert!(run(100.0, 0.0, DEFAULT_DAMPING_S / 2.0) > 30.0);
        // A rise covers the same ground three times as fast.
        let rose = run(0.0, 100.0, DEFAULT_DAMPING_S / ATTACK_RATIO);
        assert!((89.0..91.0).contains(&rose), "rise reached {}", rose);
    }
}
