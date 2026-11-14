//! Everything the PMU does not expose: temperature, fan, power, throttling,
//! PCIe link state and VRAM extent.
//!
//! Two layout generations are handled. Modern i915 exposes `gt/gt0/rps_*`;
//! older kernels expose `gt_*_freq_mhz` at the card root. The only GT attribute
//! still read here is the frequency ceiling — the clock itself comes from the
//! PMU. (`gt_cur_freq_mhz` is precisely the file nvtop misreads as the achieved
//! clock; it is the requested cap.)

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn read_u64(p: &Path) -> Option<u64> {
    read_trim(p)?.parse().ok()
}

fn read_f64(p: &Path) -> Option<f64> {
    read_trim(p)?.parse().ok()
}

pub const THROTTLE_REASONS: [(&str, &str); 8] = [
    ("throttle_reason_pl1", "PL1"),
    ("throttle_reason_pl2", "PL2"),
    ("throttle_reason_pl4", "PL4"),
    ("throttle_reason_prochot", "PROCHOT"),
    ("throttle_reason_ratl", "RATL"),
    ("throttle_reason_thermal", "THERMAL"),
    ("throttle_reason_vr_tdc", "VR-TDC"),
    ("throttle_reason_vr_thermalert", "VR-THERM"),
];

/// A PCIe link as speed and width, parsed so two links can be compared.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Link {
    pub gts: f64,
    pub width: u32,
}

impl Link {
    fn read(node: &Path, speed: &str, width: &str) -> Option<Link> {
        // `8.0 GT/s PCIe`; `Unknown` when the link is down.
        let gts: f64 = read_trim(&node.join(speed))?.split_whitespace().next()?.parse().ok()?;
        let width: u32 = read_trim(&node.join(width))?.parse().ok()?;
        if gts > 0.0 && width > 0 {
            Some(Link { gts, width })
        } else {
            None
        }
    }

    /// The best either end can do: a link trains to the lower of the two.
    fn min(self, other: Link) -> Link {
        Link { gts: self.gts.min(other.gts), width: self.width.min(other.width) }
    }

    pub fn below(&self, ceiling: &Link) -> bool {
        self.gts < ceiling.gts || self.width < ceiling.width
    }
}

impl std::fmt::Display for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1} GT/s x{}", self.gts, self.width)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Static {
    pub name: String,
    pub freq_max_mhz: u64,
    pub power_max_w: f64,
    pub temp_max_c: f64,
    pub vram_total_bytes: u64,
    /// Extent of BAR 2, the CPU's window onto device memory. Not VRAM: with
    /// resizable BAR it can exceed the memory it windows, and without it — on
    /// a platform with no ReBAR support — it is 256 MiB against, say, 6 GB.
    pub bar_bytes: u64,
    /// The fastest the host link can train: the lower of what the card's
    /// upstream port and the root port above it support. A Gen4 card in a Gen3
    /// slot has a ceiling of Gen3, and running at Gen3 is not a downtrain.
    pub link_max: Option<Link>,
    /// The card's own capability, for reference.
    pub link_dev_max: Option<Link>,
}

#[derive(Clone, Debug, Default)]
pub struct Dynamic {
    /// rc6 residency over the interval, from the hardware counter.
    pub rc6_pct: Option<f64>,
    pub temp_c: Option<f64>,
    pub fan_rpm: Option<u64>,
    pub power_w: Option<f64>,
    pub vram_used_bytes: Option<u64>,
    pub throttles: Vec<&'static str>,
    pub link_cur: Option<Link>,
}

pub struct Reader {
    card: PathBuf,
    pci: String,
    /// The card's upstream port and the root port it hangs from.
    link_nodes: Option<(PathBuf, PathBuf)>,
    gt: Option<PathBuf>,
    hwmon: Option<PathBuf>,
    last_energy: Option<(Instant, u64)>,
    /// `rc6_residency_ms`, modern or legacy location.
    rc6: Option<PathBuf>,
    last_rc6: Option<(Instant, u64)>,
    /// Last VRAM reading and when it was taken.
    vram: Option<(Instant, Option<u64>)>,
    pub stat: Static,
}

/// Where the PCI ID database may live: Debian and Proxmox ship it in
/// `/usr/share/misc`, Arch and Fedora in `/usr/share/hwdata`.
const PCI_IDS: [&str; 3] = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids", "/usr/share/pci.ids"];

/// The marketing name for an Intel device ID, from the system's `pci.ids`,
/// with an honest hex fallback rather than a guess. Hard-coding a table would
/// go stale with every new part; `pci.ids` is maintained upstream.
fn device_name(pci_id: &str) -> String {
    let id = pci_id.trim_start_matches("0x").to_ascii_lowercase();
    PCI_IDS
        .iter()
        .find_map(|p| fs::read_to_string(p).ok())
        .and_then(|db| pci_ids_name(&db, &id))
        .unwrap_or_else(|| format!("Intel GPU {}", pci_id))
}

/// Looks `device` up under vendor 8086 in a `pci.ids` body. Entries read like
/// `DG2 [Arc A380]` or `CoffeeLake-S GT2 [UHD Graphics 630]`; the bracketed
/// product name leads, with the codename after it.
fn pci_ids_name(db: &str, device: &str) -> Option<String> {
    let mut in_intel = false;
    for line in db.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if !line.starts_with('\t') {
            // A vendor line, or the device-class section that ends the list.
            if in_intel {
                return None;
            }
            in_intel = line.starts_with("8086 ");
            continue;
        }
        if !in_intel || line.starts_with("\t\t") {
            continue;
        }
        let entry = &line[1..];
        if let Some(name) = entry.strip_prefix(device).and_then(|r| r.strip_prefix("  ")) {
            let name = name.trim();
            return Some(match (name.find('['), name.rfind(']')) {
                (Some(a), Some(b)) if b > a => {
                    let code = name[..a].trim();
                    let product = &name[a + 1..b];
                    if code.is_empty() { product.to_string() } else { format!("{} ({})", product, code) }
                }
                _ => name.to_string(),
            });
        }
    }
    None
}

impl Reader {
    pub fn new(card: PathBuf) -> Self {
        let gt = {
            let p = card.join("gt/gt0");
            if p.is_dir() { Some(p) } else { None }
        };
        let hwmon = fs::read_dir(card.join("device/hwmon"))
            .ok()
            .and_then(|mut d| d.next().and_then(|e| e.ok()).map(|e| e.path()));

        let pci = fs::canonicalize(card.join("device"))
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
            .unwrap_or_default();
        let link_nodes = host_link_nodes(&card.join("device"));
        let rc6 = gt
            .as_ref()
            .map(|g| g.join("rc6_residency_ms"))
            .into_iter()
            .chain(std::iter::once(card.join("power/rc6_residency_ms")))
            .find(|p| p.is_file());
        let mut r = Reader {
            card,
            pci,
            link_nodes,
            gt,
            hwmon,
            last_energy: None,
            rc6,
            last_rc6: None,
            vram: None,
            stat: Static::default(),
        };
        r.load_static();
        r
    }

    /// Reads a GT attribute, preferring the modern `gt/gt0/rps_*` name and
    /// falling back to the legacy `gt_*` file at the card root.
    fn gt_attr(&self, modern: &str, legacy: &str) -> Option<u64> {
        if let Some(gt) = &self.gt {
            if let Some(v) = read_u64(&gt.join(modern)) {
                return Some(v);
            }
        }
        read_u64(&self.card.join(legacy))
    }

    fn load_static(&mut self) {
        let dev = self.card.join("device");
        let pci_id = read_trim(&dev.join("device")).unwrap_or_else(|| "0x0000".into());
        self.stat.name = device_name(&pci_id);

        self.stat.freq_max_mhz = self.gt_attr("rps_max_freq_mhz", "gt_max_freq_mhz").unwrap_or(0);

        if let Some(h) = &self.hwmon {
            // power1_max is a microwatt cap. nvtop presents this as if it were a
            // live reading; it is not, and it never changes.
            self.stat.power_max_w = read_f64(&h.join("power1_max")).map(|v| v / 1e6).unwrap_or(0.0);

            // Intel publishes no TjMax for its Arc cards — it is absent from ARK,
            // from the Arc thermal article, and from the driver, which exposes
            // neither temp1_max nor temp1_crit. So prefer a real limit if the
            // kernel has one, and otherwise fall back to 90 °C: the figure Intel's
            // own support article treats as warranting action. It is a *display scale*,
            // not a vendor specification, and must not be quoted as one. The
            // authoritative overheating signal is throttle_reason_thermal.
            self.stat.temp_max_c = read_f64(&h.join("temp1_crit"))
                .or_else(|| read_f64(&h.join("temp1_max")))
                .map(|v| v / 1000.0)
                .filter(|v| *v > 0.0)
                .unwrap_or(90.0);
        }

        if let Some((up, root)) = &self.link_nodes {
            let dev_max = Link::read(up, "max_link_speed", "max_link_width");
            let root_max = Link::read(root, "max_link_speed", "max_link_width");
            self.stat.link_dev_max = dev_max;
            self.stat.link_max = match (dev_max, root_max) {
                (Some(d), Some(r)) => Some(d.min(r)),
                (d, r) => d.or(r),
            };
        }

        self.stat.bar_bytes = bar_size(&dev, 2).unwrap_or(0);
        self.stat.vram_total_bytes = vram_debugfs(&self.pci).map(|(_, t)| t).unwrap_or(0);
    }

    /// Drops the energy baseline, so power after a pause is not averaged over
    /// the pause.
    pub fn reset(&mut self) {
        self.last_energy = None;
        self.last_rc6 = None;
    }

    pub fn sample(&mut self) -> Dynamic {
        let mut d = Dynamic::default();

        // rc6 comes from the hardware counter here, not from the PMU. On kernel
        // 7.0 with an Arc A380 under GuC SLPC, the PMU's `rc6-residency` did not
        // move at all over an idle card — 0 ns in 3 s, opened alone — while this
        // file advanced 3,000 ms over the same 3 s. i915_pmu.c's
        // get_rc6() never reports less than it last reported, so a parked-time
        // estimate that once ran ahead of the hardware freezes the counter until
        // the hardware catches up (inferred from the source, not traced).
        if let Some(p) = &self.rc6 {
            if let Some(ms) = read_u64(p) {
                let now = Instant::now();
                if let Some((t0, ms0)) = self.last_rc6 {
                    let dt = now.duration_since(t0).as_secs_f64();
                    if dt > 0.0 && ms >= ms0 {
                        d.rc6_pct = Some(((ms - ms0) as f64 / 1e3 / dt * 100.0).clamp(0.0, 100.0));
                    }
                }
                self.last_rc6 = Some((now, ms));
            }
        }

        if let Some(h) = &self.hwmon {
            d.temp_c = read_f64(&h.join("temp1_input")).map(|v| v / 1000.0);
            d.fan_rpm = read_u64(&h.join("fan1_input"));

            // i915 publishes no power1_input on discrete cards, only a
            // cumulative energy counter in microjoules, so draw must be
            // differentiated. A kernel that does provide an instantaneous
            // reading is used as such.
            if let Some(uw) = read_f64(&h.join("power1_input")) {
                d.power_w = Some(uw / 1e6);
            } else if let Some(uj) = read_u64(&h.join("energy1_input")) {
                let now = Instant::now();
                if let Some((t0, e0)) = self.last_energy {
                    let dt = now.duration_since(t0).as_secs_f64();
                    if dt > 0.0 && uj >= e0 {
                        d.power_w = Some((uj - e0) as f64 / dt / 1e6);
                    }
                }
                self.last_energy = Some((now, uj));
            }
        }

        if let Some(gt) = &self.gt {
            // `throttle_reason_status` is the OR of every reason, so it is read
            // first and the eight named files only when it is set — one
            // register read per sample instead of nine in the usual, unthrottled
            // case. In mainline its mask (GT0_PERF_LIMIT_REASONS_MASK, 0xde3) is
            // exactly the eight bits named above, so `OTHER` cannot fire there;
            // it stays so that throttled for an unnamed reason, on a kernel that
            // widens the mask, still reads as throttled rather than "none". A
            // kernel without the status file falls back to reading all eight.
            let status = read_trim(&gt.join("throttle_reason_status")).map(|v| v != "0");
            if status != Some(false) {
                for (file, label) in THROTTLE_REASONS {
                    if read_trim(&gt.join(file)).map(|v| v != "0").unwrap_or(false) {
                        d.throttles.push(label);
                    }
                }
                if d.throttles.is_empty() && status == Some(true) {
                    d.throttles.push("OTHER");
                }
            }
        }

        d.link_cur = self
            .link_nodes
            .as_ref()
            .and_then(|(up, _)| Link::read(up, "current_link_speed", "current_link_width"));
        // `i915_gem_objects` walks the buddy allocator's free lists under its
        // lock, and VRAM occupancy moves slowly, so it is refreshed at most once
        // a second whatever the sampling rate.
        let now = Instant::now();
        if self.vram.map(|(t, _)| now.duration_since(t).as_secs_f64() >= 1.0).unwrap_or(true) {
            self.vram = Some((now, vram_debugfs(&self.pci).map(|(u, _)| u)));
        }
        d.vram_used_bytes = self.vram.and_then(|(_, u)| u);
        d
    }
}

/// The link worth reporting is the card's *host* link, not its internal one.
///
/// Intel Arc boards carry an integrated PCIe switch, so the GPU function
/// describes only the switch's internal downstream port and reports a
/// meaningless "2.5GT/s x1" — alarming to read and wrong. The real link is at
/// the card's upstream port: the node whose parent is a root port, i.e. whose
/// grandparent is the `pciDDDD:BB` host bridge. Matching on the host bridge
/// rather than on bus `00` keeps this right on a root complex numbered from any
/// bus or domain. Returns the upstream port and the root port. An integrated
/// GPU sits directly on the root bus and has no such link at all, hence `None`.
fn host_link_nodes(dev: &Path) -> Option<(PathBuf, PathBuf)> {
    let is_host_bridge = |p: &Path| {
        p.file_name().map(|n| n.to_string_lossy().starts_with("pci")).unwrap_or(true)
    };
    let mut node = fs::canonicalize(dev).ok()?;
    loop {
        let parent = node.parent()?.to_path_buf();
        if is_host_bridge(&parent) {
            return None; // already on the root bus: no upstream link of its own
        }
        if is_host_bridge(parent.parent()?) {
            return Some((node, parent));
        }
        node = parent;
    }
}

const DEBUGFS_DRI: &str = "/sys/kernel/debug/dri";

/// Reads a DRM debugfs file for the device at `pci`.
///
/// Recent kernels name the directory after the PCI address
/// (`dri/0000:03:00.0`). Older ones — 6.1, for one — have only numbered
/// directories (`dri/0`, `dri/128`), each with a `name` file reading
/// `i915 dev=0000:03:00.0 unique=…`; the first of those holding `file` is used.
/// Root-only, like the PMU.
pub fn read_debugfs(pci: &str, file: &str) -> Option<String> {
    if pci.is_empty() {
        return None;
    }
    let direct = Path::new(DEBUGFS_DRI).join(pci).join(file);
    if let Ok(body) = fs::read_to_string(&direct) {
        return Some(body);
    }
    let mut dirs: Vec<(u32, PathBuf)> = fs::read_dir(DEBUGFS_DRI)
        .ok()?
        .flatten()
        .filter_map(|e| Some((e.file_name().to_string_lossy().parse().ok()?, e.path())))
        .collect();
    dirs.sort();
    let tag = format!("dev={}", pci);
    dirs.into_iter()
        .filter(|(_, d)| {
            read_trim(&d.join("name")).map(|n| n.split_whitespace().any(|t| t == tag)).unwrap_or(false)
        })
        .find_map(|(_, d)| fs::read_to_string(d.join(file)).ok())
}

/// True device memory, from debugfs. Within the `local0` block `size` counts
/// 4 KiB pages and `usage` is already in bytes. Integrated GPUs have no
/// `local0` region and read `None`.
pub fn vram_debugfs(pci: &str) -> Option<(u64, u64)> {
    parse_gem_objects(&read_debugfs(pci, "i915_gem_objects")?)
}

fn parse_gem_objects(body: &str) -> Option<(u64, u64)> {
    let mut in_local = false;
    let (mut total, mut used) = (0u64, 0u64);
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with("local0:") {
            in_local = true;
            continue;
        }
        if !in_local {
            continue;
        }
        if let Some(v) = t.strip_prefix("size:") {
            total = v.trim().parse::<u64>().ok()?.saturating_mul(4096);
        } else if let Some(v) = t.strip_prefix("usage:") {
            used = v.trim().parse().ok()?;
            break;
        }
    }
    if total > 0 {
        Some((used, total))
    } else {
        None
    }
}

/// Extent of a PCI BAR, from the `resource` table. Each line is
/// `start end flags` in hex; a zero-length entry means the BAR is unpopulated.
fn bar_size(dev: &Path, index: usize) -> Option<u64> {
    let body = fs::read_to_string(dev.join("resource")).ok()?;
    let line = body.lines().nth(index)?;
    let mut it = line.split_whitespace();
    let start = u64::from_str_radix(it.next()?.trim_start_matches("0x"), 16).ok()?;
    let end = u64::from_str_radix(it.next()?.trim_start_matches("0x"), 16).ok()?;
    if end > start {
        Some(end - start + 1)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PCI_IDS: &str = "# comment\n\
8086  Intel Corporation\n\
\t3e92  CoffeeLake-S GT2 [UHD Graphics 630]\n\
\t\t1028 0869  Vostro 3470\n\
\t56a5  DG2 [Arc A380]\n\
\te20b  Battlemage G21 [Arc B580]\n\
\t9a49  TigerLake-LP GT2 [Iris Xe Graphics]\n\
\t1234  Plain Name\n\
8087  Intel Corporation\n\
\t56a5  Not this one\n\
C 00  Unclassified device\n";

    #[test]
    fn names_come_from_pci_ids() {
        assert_eq!(pci_ids_name(PCI_IDS, "56a5").as_deref(), Some("Arc A380 (DG2)"));
        assert_eq!(pci_ids_name(PCI_IDS, "3e92").as_deref(), Some("UHD Graphics 630 (CoffeeLake-S GT2)"));
        assert_eq!(pci_ids_name(PCI_IDS, "1234").as_deref(), Some("Plain Name"));
        assert_eq!(pci_ids_name(PCI_IDS, "0869"), None, "subsystem lines are not devices");
        assert_eq!(pci_ids_name(PCI_IDS, "ffff"), None);
    }

    #[test]
    fn gem_objects_local_memory() {
        // Shape as printed by kernel 7.0; the figures are made up.
        let body = "12 shrinkable [0 free] objects, 1048576 bytes\n\
                    system-ttm: total:0x0000000f00000000 bytes\n\
                    local0:   use_type: 1\n  use_tt: 0\n  size: 1572864\n  usage: 268435456\n\
                    default_page_size: 64KiB\nvisible_size: 256MiB\n";
        assert_eq!(parse_gem_objects(body), Some((256 << 20, 6 << 30)));
        // An integrated GPU has no local0 region.
        assert_eq!(parse_gem_objects("3 shrinkable [0 free] objects, 4096 bytes\n"), None);
    }
}
