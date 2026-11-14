//! Per-process DRM accounting via `/proc/<pid>/fdinfo`.
//!
//! Two things make this worth doing properly rather than the way nvtop does it.
//!
//! First, a client that holds several file descriptors on the same render node
//! reports identical cumulative counters on every one of them, so summing over
//! descriptors multiplies the answer. Entries are therefore deduplicated on
//! `(drm-pdev, drm-client-id)`.
//!
//! Second, a transcoder running inside a container is visible in the host PID
//! namespace, so the host can see it without entering the container. Its cgroup
//! line names the container, which turns an anonymous PID into a labelled row.
//!
//! Third, *finding* the clients is the expensive part. Walking every process's
//! `fd/` directory cost ~0.1 s of CPU per scan on a busy virtualisation host
//! (1,500 processes, 24,500 descriptors) — two thirds of igtop's total — to
//! find what is usually zero or two clients. The DRM core already keeps that list in
//! debugfs, so it is read first and only the processes it names are walked. The
//! full walk remains as the fallback when debugfs is unavailable.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Client {
    pub pid: i32,
    pub comm: String,
    pub container: Option<String>,
    pub client_id: u64,
    pub pdev: String,
    /// Cumulative busy nanoseconds keyed by the fdinfo engine name
    /// (`render`, `copy`, `video`, `video-enhance`, `compute`).
    pub engines: HashMap<String, u64>,
    /// Engine-class capacity, e.g. `video: 2` on a part with vcs0 and vcs1.
    /// fdinfo reports the *sum* across the class, so a percentage derived from
    /// it must be divided by this to be comparable with one engine's figure.
    pub capacity: HashMap<String, u64>,
    pub mem_resident: u64,
    /// Bytes in buffers that also have a handle in another client
    /// (`drm-shared-*`). Counted in `mem_resident` too, by every client
    /// holding them.
    pub mem_shared: u64,
}

/// A client with its engine figures converted to percentages over an interval.
#[derive(Clone, Debug)]
pub struct ClientRate {
    pub pid: i32,
    pub comm: String,
    pub container: Option<String>,
    pub mem_resident: u64,
    mem_shared: u64,
    pub engines: HashMap<String, f64>,
    pub busiest: f64,
}

/// Client rates are averaged over this many seconds rather than over one scan.
///
/// With GuC submission i915 updates a context's runtime from its saved image,
/// which is written when the context is switched out, so over a one-second
/// window a busy client reads in bursts — 3.1 %, 0 %, 0 % in three 5 s windows
/// of a live transcode, while `vecs0` read 8–16 %. Over ten seconds
/// the bursts average out. The PMU bars remain the live, authoritative figure.
pub const WINDOW_S: u64 = 10;

fn container_of(pid: i32) -> Option<String> {
    container_from_cgroup(&fs::read_to_string(format!("/proc/{}/cgroup", pid)).ok()?)
}

/// Names the container or guest a process belongs to from its cgroup file.
///
/// - Proxmox: `/lxc/<vmid>` for containers, `/qemu.slice/<vmid>.scope` for VMs.
/// - Docker: `/docker/<id>` (cgroupfs) or `docker-<id>.scope` (systemd).
/// - Podman: `libpod-<id>.scope`.
/// - systemd-nspawn and machinectl: `machine-<name>.scope`.
///
/// Container IDs are shortened to the 12 characters `docker ps` shows.
///
/// The path is everything after the *second* colon, taken whole. Splitting on
/// the last colon instead would let a container that manages its own cgroup
/// subtree name a child `x:/lxc/999` and be labelled as a different guest.
fn container_from_cgroup(body: &str) -> Option<String> {
    let short = |id: &str| id.chars().take(12).collect::<String>();
    for line in body.lines() {
        let path = match line.splitn(3, ':').nth(2) {
            Some(p) => p,
            None => continue,
        };
        if let Some(rest) = path.strip_prefix("/lxc/") {
            let id = rest.split('/').next().unwrap_or("");
            if !id.is_empty() {
                return Some(format!("CT {}", id));
            }
        }
        if let Some(idx) = path.find("/qemu.slice/") {
            let rest = &path[idx + "/qemu.slice/".len()..];
            if let Some(id) = rest.split('/').next().and_then(|s| s.strip_suffix(".scope")) {
                return Some(format!("VM {}", id));
            }
        }
        for seg in path.split('/') {
            if let Some(id) = seg.strip_prefix("docker-").and_then(|s| s.strip_suffix(".scope")) {
                return Some(format!("docker {}", short(id)));
            }
            if let Some(id) = seg.strip_prefix("libpod-").and_then(|s| s.strip_suffix(".scope")) {
                // `libpod-conmon-<id>.scope` is the monitor, not the container.
                if !id.starts_with("conmon-") {
                    return Some(format!("podman {}", short(id)));
                }
            }
            if let Some(name) = seg.strip_prefix("machine-").and_then(|s| s.strip_suffix(".scope")) {
                // systemd escapes `-` in unit names as `\x2d`.
                return Some(name.replace("\\x2d", "-"));
            }
        }
        if let Some(rest) = path.strip_prefix("/docker/") {
            let id = rest.split('/').next().unwrap_or("");
            if !id.is_empty() {
                return Some(format!("docker {}", short(id)));
            }
        }
    }
    None
}

/// fdinfo memory values are rendered as `1520 KiB`, occasionally as a bare
/// byte count. Both forms are accepted.
fn parse_size(v: &str) -> u64 {
    let mut it = v.split_whitespace();
    let n: f64 = match it.next().and_then(|s| s.parse().ok()) {
        Some(n) => n,
        None => return 0,
    };
    let mult = match it.next() {
        Some("KiB") => 1024.0,
        Some("MiB") => 1024.0 * 1024.0,
        Some("GiB") => 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    (n * mult) as u64
}

fn parse_fdinfo(path: &Path) -> Option<Client> {
    parse_fdinfo_body(&fs::read_to_string(path).ok()?)
}

fn parse_fdinfo_body(body: &str) -> Option<Client> {
    if !body.contains("drm-client-id") {
        return None;
    }
    let mut c = Client::default();
    let (mut resident, mut shared) = (0u64, 0u64);
    for line in body.lines() {
        let (k, v) = match line.split_once(':') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        match k {
            "drm-client-id" => c.client_id = v.parse().unwrap_or(0),
            "drm-pdev" => c.pdev = v.to_string(),
            _ => {
                if let Some(eng) = k.strip_prefix("drm-engine-") {
                    // `drm-engine-capacity-video: 2` is a count, not a duration.
                    if let Some(class) = eng.strip_prefix("capacity-") {
                        c.capacity.insert(class.to_string(), v.parse().unwrap_or(1).max(1));
                    } else {
                        // Values arrive as `8310000000 ns`.
                        let ns = v.split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(0);
                        c.engines.insert(eng.to_string(), ns);
                    }
                } else if k.starts_with("drm-resident-") {
                    resident += parse_size(v);
                } else if k.starts_with("drm-shared-") {
                    shared += parse_size(v);
                }
            }
        }
    }
    c.mem_resident = resident;
    c.mem_shared = shared;
    Some(c)
}

/// Process IDs holding the device open, from the DRM core's own client list
/// (`/sys/kernel/debug/dri/<pci>/clients`). `None` when it cannot be read, which
/// sends the caller to the full walk.
///
/// Columns are located from the *right*: `command` is the task's comm and may
/// contain spaces (`Plex Transcoder`, say), whereas every later column cannot — the
/// client name is rejected by the kernel unless every byte is `isgraph`. The
/// header is used to find the `tgid` column's offset, so a kernel that adds or
/// drops a trailing column is still read correctly. The PID is the one that last
/// used the client (`drm_file_update_pid`), in the reader's namespace — for
/// root on the host, the host PID.
fn debugfs_client_pids(pdev: &str) -> Option<HashSet<i32>> {
    parse_debugfs_clients(&crate::sysfs::read_debugfs(pdev, "clients")?)
}

fn parse_debugfs_clients(body: &str) -> Option<HashSet<i32>> {
    let mut lines = body.lines();
    let header: Vec<&str> = lines.next()?.split_whitespace().collect();
    let col = header.iter().position(|h| *h == "tgid")?;
    let from_end = header.len() - col;
    let mut pids = HashSet::new();
    for line in lines {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() < from_end {
            continue;
        }
        if let Ok(p) = t[t.len() - from_end].parse::<i32>() {
            if p > 0 {
                pids.insert(p);
            }
        }
    }
    Some(pids)
}

/// Scans processes for open DRM descriptors on `pdev`, or on any device when
/// `pdev` is `None`. Reading `fd/` first and only then the matching `fdinfo/`
/// entry keeps the per-process cost proportional to the descriptors actually
/// pointing at a GPU.
pub fn scan(pdev: Option<&str>) -> Vec<Client> {
    let candidates: Option<HashSet<i32>> = pdev.and_then(debugfs_client_pids);
    let pids: Vec<i32> = match candidates {
        Some(set) => set.into_iter().collect(),
        None => match fs::read_dir("/proc") {
            Ok(p) => p
                .flatten()
                .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
                .collect(),
            Err(_) => return Vec::new(),
        },
    };

    let mut found: HashMap<(String, u64), Client> = HashMap::new();

    for pid in pids {
        let proc_dir = Path::new("/proc").join(pid.to_string());
        let fd_dir = match fs::read_dir(proc_dir.join("fd")) {
            Ok(d) => d,
            // Processes disappear mid-scan, and some are unreadable. Both are
            // normal; neither is worth reporting.
            Err(_) => continue,
        };

        // Resolved once per process, on its first DRM descriptor.
        let mut ident: Option<(String, Option<String>)> = None;

        for fd in fd_dir.flatten() {
            let target = match fs::read_link(fd.path()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if !target.to_string_lossy().starts_with("/dev/dri/") {
                continue;
            }
            let info = proc_dir.join("fdinfo").join(fd.file_name());
            let mut c = match parse_fdinfo(&info) {
                Some(c) => c,
                None => continue,
            };
            if pdev.map(|f| c.pdev != f).unwrap_or(false) {
                continue;
            }

            let (comm, container) = ident.get_or_insert_with(|| {
                (
                    fs::read_to_string(proc_dir.join("comm"))
                        .map(|s| s.trim().to_string())
                        .unwrap_or_else(|_| "?".into()),
                    container_of(pid),
                )
            });
            c.pid = pid;
            c.comm = comm.clone();
            c.container = container.clone();

            // Same client seen twice: keep the larger counters rather than
            // adding, since both descriptors report the same cumulative totals.
            let key = (c.pdev.clone(), c.client_id);
            match found.get_mut(&key) {
                Some(prev) => {
                    for (eng, ns) in &c.engines {
                        let e = prev.engines.entry(eng.clone()).or_insert(0);
                        if *ns > *e {
                            *e = *ns;
                        }
                    }
                    for (cls, cap) in &c.capacity {
                        prev.capacity.insert(cls.clone(), *cap);
                    }
                    prev.mem_resident = prev.mem_resident.max(c.mem_resident);
                    prev.mem_shared = prev.mem_shared.max(c.mem_shared);
                }
                None => {
                    found.insert(key, c);
                }
            }
        }
    }

    found.into_values().collect()
}

/// Clients keyed by `(drm-pdev, drm-client-id)`.
type ClientMap = HashMap<(String, u64), Client>;

/// Folds one process's clients into a single row. Engine time adds.
///
/// Memory does not simply add. i915 counts a buffer in `drm-resident-*` of
/// every client holding a handle to it, and in `drm-shared-*` as well; a media
/// server's VA-API decode and OpenCL tonemap share their surfaces this way, so a
/// plain sum counts those surfaces twice, while the maximum misses the second client's private buffers. Every client's private memory is
/// therefore added and the shared memory counted once, as the largest client's
/// shared figure — an estimate, since which buffers overlap is not published,
/// floored at the largest single client, which is certainly resident.
fn fold_process(mut rs: Vec<ClientRate>) -> ClientRate {
    let mut acc = rs.swap_remove(0);
    if rs.is_empty() {
        return acc;
    }
    let (mut sum_res, mut sum_shared) = (acc.mem_resident, acc.mem_shared);
    let (mut max_res, mut max_shared) = (acc.mem_resident, acc.mem_shared);
    for r in rs {
        for (eng, pct) in &r.engines {
            let e = acc.engines.entry(eng.clone()).or_insert(0.0);
            *e = (*e + *pct).min(100.0);
        }
        sum_res += r.mem_resident;
        sum_shared += r.mem_shared;
        max_res = max_res.max(r.mem_resident);
        max_shared = max_shared.max(r.mem_shared);
    }
    acc.mem_resident = sum_res.saturating_sub(sum_shared - max_shared).max(max_res);
    acc.mem_shared = max_shared;
    acc
}

/// Differentiates scans into per-engine utilisation percentages, each client
/// measured against its oldest scan within `WINDOW_S`.
pub struct ClientTracker {
    /// Recent scans, oldest first. The front is the newest scan at least
    /// `WINDOW_S` old, or the oldest held if none is that old yet.
    scans: VecDeque<(Instant, ClientMap)>,
}

impl ClientTracker {
    pub fn new() -> Self {
        ClientTracker { scans: VecDeque::new() }
    }

    /// Forgets every previous scan. Called when scanning resumes after a gap —
    /// a pause, or the table being hidden — so the first rates after it are not
    /// averaged across the whole gap.
    pub fn reset(&mut self) {
        self.scans.clear();
    }

    pub fn update(&mut self, pdev_filter: Option<&str>) -> Vec<ClientRate> {
        let now = Instant::now();
        let current: ClientMap = scan(pdev_filter)
            .into_iter()
            .map(|c| ((c.pdev.clone(), c.client_id), c))
            .collect();

        // Keep one scan at or beyond the window's edge as the baseline; drop
        // anything older.
        let window = Duration::from_secs(WINDOW_S);
        while self.scans.len() >= 2 && now.duration_since(self.scans[1].0) >= window {
            self.scans.pop_front();
        }

        // A client absent from every held scan (start-up, after a reset, or
        // newly opened) is listed at zero for one interval, rather than with its
        // entire lifetime's work compressed into one tick. Returning nothing
        // instead would flash "idle — no process holds this GPU" in the middle
        // of a transcode. A client that appeared within the window is measured
        // from its first sighting.
        let mut out = Vec::new();
        for (key, c) in &current {
            let base = self
                .scans
                .iter()
                .find_map(|(t, m)| m.get(key).map(|p| (now.duration_since(*t).as_secs_f64() * 1e9, p)));
            let mut engines = HashMap::new();
            for (eng, ns) in &c.engines {
                let pct = match base {
                    Some((dt_ns, p)) if dt_ns > 0.0 => {
                        let b = p.engines.get(eng).copied().unwrap_or(*ns);
                        // Divided by class capacity: with two video engines the
                        // counter advances at up to twice wall-clock, which
                        // would otherwise read as a permanently pegged 100%.
                        let cap = c.capacity.get(eng).copied().unwrap_or(1).max(1) as f64;
                        (ns.saturating_sub(b) as f64 / dt_ns / cap * 100.0).clamp(0.0, 100.0)
                    }
                    _ => 0.0,
                };
                engines.insert(eng.clone(), pct);
            }
            out.push(ClientRate {
                pid: c.pid,
                comm: c.comm.clone(),
                container: c.container.clone(),
                mem_resident: c.mem_resident,
                mem_shared: c.mem_shared,
                engines,
                busiest: 0.0,
            });
        }

        self.scans.push_back((now, current));

        // One process can hold several DRM clients: a transcoder may use one
        // for VA-API decode and a second for the OpenCL tonemap, which otherwise renders as
        // two rows that look identical. Fold them into one row per process.
        let mut by_proc: HashMap<i32, Vec<ClientRate>> = HashMap::new();
        for r in out {
            by_proc.entry(r.pid).or_default().push(r);
        }
        let mut out: Vec<ClientRate> = by_proc.into_values().map(fold_process).collect();
        for r in out.iter_mut() {
            r.busiest = r.engines.values().copied().fold(0.0_f64, f64::max);
        }

        out.sort_by(|a, b| {
            b.busiest
                .partial_cmp(&a.busiest)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.pid.cmp(&b.pid))
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FDINFO: &str = "pos:\t0\nflags:\t02100002\nmnt_id:\t26\n\
drm-driver:\ti915\ndrm-client-id:\t7\ndrm-pdev:\t0000:03:00.0\n\
drm-total-system0:\t4 MiB\ndrm-shared-system0:\t0\ndrm-resident-system0:\t4 MiB\n\
drm-total-local0:\t120 MiB\ndrm-shared-local0:\t8 MiB\ndrm-resident-local0:\t120 MiB\n\
drm-engine-render:\t0 ns\ndrm-engine-copy:\t0 ns\n\
drm-engine-video:\t8310000000 ns\ndrm-engine-capacity-video:\t2\n\
drm-engine-video-enhance:\t1200 ns\ndrm-engine-capacity-video-enhance:\t2\n";

    #[test]
    fn fdinfo_is_parsed() {
        let c = parse_fdinfo_body(FDINFO).unwrap();
        assert_eq!(c.client_id, 7);
        assert_eq!(c.pdev, "0000:03:00.0");
        assert_eq!(c.engines["video"], 8_310_000_000);
        assert_eq!(c.engines["video-enhance"], 1200);
        assert_eq!(c.engines["render"], 0);
        assert_eq!(c.capacity["video"], 2);
        assert!(!c.engines.contains_key("capacity-video"));
        assert_eq!(c.mem_resident, 124 << 20);
        assert_eq!(c.mem_shared, 8 << 20);
    }

    #[test]
    fn non_drm_fdinfo_is_ignored() {
        assert!(parse_fdinfo_body("pos:\t0\nflags:\t02\nmnt_id:\t26\n").is_none());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1520 KiB"), 1520 * 1024);
        assert_eq!(parse_size("3 MiB"), 3 << 20);
        assert_eq!(parse_size("2 GiB"), 2 << 30);
        assert_eq!(parse_size("4096"), 4096);
        assert_eq!(parse_size("junk"), 0);
    }

    #[test]
    fn debugfs_clients_read_from_the_right() {
        // Older kernels end at `name`; newer ones add an `id` column.
        let old = "             command  tgid dev master a   uid      magic        name\n\
                   \x20               Xorg  1234   0   y    y     0          0     <unset>\n\
                   \x20    Plex Transcoder  5678 128   n    n     0          0     <unset>\n";
        let new = "             command  tgid dev master a   uid      magic        name    id\n\
                   \x20             ffmpeg  4321 128   n    n  1000          0     <unset>    17\n\
                   \x20             ffmpeg  4321 128   n    n  1000          0     <unset>    18\n";
        assert_eq!(parse_debugfs_clients(old).unwrap(), [1234, 5678].into_iter().collect());
        assert_eq!(parse_debugfs_clients(new).unwrap(), [4321].into_iter().collect());
    }

    #[test]
    fn containers_are_named() {
        let cases = [
            ("0::/lxc/101/ns/init.scope\n", Some("CT 101")),
            ("0::/qemu.slice/200.scope\n", Some("VM 200")),
            (
                "0::/system.slice/docker-0123456789abcdef0123.scope\n",
                Some("docker 0123456789ab"),
            ),
            ("12:memory:/docker/fedcba9876543210ff\n", Some("docker fedcba987654")),
            ("0::/machine.slice/libpod-aaaabbbbccccdddd.scope/container\n", Some("podman aaaabbbbcccc")),
            ("0::/machine.slice/libpod-conmon-aaaabbbb.scope\n", None),
            ("0::/machine.slice/machine-my\\x2dbox.scope/payload\n", Some("my-box")),
            ("0::/user.slice/user-1000.slice/session-2.scope\n", None),
            // A path smuggled in after a further colon is not taken as the guest.
            ("0::/system.slice/foo:/lxc/999\n", None),
        ];
        for (body, want) in cases {
            assert_eq!(container_from_cgroup(body).as_deref(), want, "{}", body);
        }
    }
}
