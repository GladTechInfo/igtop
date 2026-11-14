//! igtop — an i915 GPU monitor built on the i915 PMU.
//!
//! Written because nvtop misreports Intel Arc cards in several independent ways:
//! it reads `gt_cur_freq_mhz` (the requested cap) as though it were the achieved
//! clock, never opens the PMU so reports no utilisation at all, presents the
//! static `power1_max` cap as a live figure, and shows itself in the process
//! list whilst missing the transcoder inside the container.

mod app;
mod clients;
mod perf;
mod pmu;
mod sysfs;
mod theme;
mod ui;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::io;
use std::time::{Duration, Instant};

struct Args {
    interval_ms: u64,
    once: bool,
    list: bool,
    device: Option<String>,
    no_clients: bool,
    no_smooth: bool,
    no_icons: bool,
    damping: f64,
    fps: u64,
    focus: Option<usize>,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            interval_ms: 1000,
            once: false,
            list: false,
            device: None,
            no_clients: false,
            no_smooth: false,
            no_icons: false,
            damping: app::DEFAULT_DAMPING_S,
            // Frame rate is nearly all of igtop's cost (4.8% of a core at 60
            // fps, 2.7% at 30, measured during a transcode), and halving it
            // costs little visible smoothness.
            fps: 30,
            focus: None,
        }
    }
}

const USAGE: &str = "\
igtop — i915 / Intel Arc GPU monitor

USAGE:
    igtop [OPTIONS]

OPTIONS:
    -i, --interval <MS>   refresh interval in milliseconds (default 1000, 50-10000)
    -d, --device <PCI>    restrict to one card: 0000:BB:DD.F, or the short BB:DD.F
        --once            print a single plain-text snapshot and exit
        --list            list detected GPUs and exit
        --no-clients      skip the per-process scan
        --no-smooth       show raw values, undamped
        --no-icons        plain ASCII markers instead of Nerd Font glyphs
        --damping <SEC>   seconds for a bar to fall ~90% (default 4, min 0.2)
        --fps <N>         animation frame rate (default 30, 5-240)
    -g, --focus <N>       start showing only GPU N (see --list order)
    -h, --help            this text

KEYS:
    q / Esc   quit          c   toggle client table
    space     pause         h   toggle history
    i         toggle icons  s   toggle damping
    + / -     refresh rate: 50 100 250 500 1000 2000 5000 10000 ms
    g         cycle which GPU is shown: all -> first -> second -> all

REDRAW:
    Sampling and drawing are separate. Every sample (-i, or + / -) redraws, so
    temperature, clock and power always update at the sampling rate. Between
    samples the screen is redrawn at up to --fps only while a bar is still
    gliding, and a frame is skipped when no bar would move by a visible step
    (an eighth of a cell, or 0.1 in a figure). Once everything has settled,
    igtop sleeps until the next sample or key. With damping off, only
    samples redraw.

DAMPING:
    Bars are damped for display with a fast attack and slow decay, so a spike
    shows at once but the bar settles instead of flickering. The underlying
    values are untouched: --once, the raw history and every numeric sensor
    are always raw.

HISTORY:
    One column per second, whatever the refresh rate: the mean busy figure
    over that second. Ten minutes are kept.

CLIENTS:
    Engine figures are averaged over the last 10 seconds. i915 credits a
    context's time in bursts, so a one-second figure jumps about; the engine
    bars are the live measure. A client holding a buffer shared with another
    client of the same process is counted once for it in MEM.

CLOCK:
    The kernel integrates 0 MHz while the GPU sleeps, so the plain mean over an
    interval is diluted by idle time. CLOCK and freq_act_mhz are therefore the
    mean *while awake*; --once also prints the diluted wall-time mean as
    freq_act_avg_mhz, which is what intel_gpu_top shows.

--ONCE FORMAT:
    GPU lines are `key value`, the value running to end of line. Client lines
    are `key=value` pairs; a value containing spaces, quotes or control
    characters is double-quoted with \\\\ \\\" and \\xNN escapes. Process names
    come from inside containers and are never printed raw.

NOTE:
    The i915 PMU is a system-wide event source. With perf_event_paranoid at 4
    this must run as root. Do not lower that sysctl to work around it.
";

/// What `+` and `-` step through. Halving and doubling did not return to 1000:
/// 1000 → 500 → … → 62 → 50, then 100 → 200 → … → 800 → 1600. A value set with
/// `-i` that is not on the ladder steps to its nearest neighbour.
const INTERVAL_STEPS: [u64; 8] = [50, 100, 250, 500, 1000, 2000, 5000, 10_000];

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{}", USAGE);
                std::process::exit(0);
            }
            "-i" | "--interval" => {
                let v = it.next().ok_or("--interval needs a value")?;
                a.interval_ms =
                    v.parse::<u64>().map_err(|_| "--interval must be a number")?.clamp(50, 10_000);
            }
            "-d" | "--device" => {
                a.device = Some(normalise_pci(&it.next().ok_or("--device needs a value")?));
            }
            "--once" => a.once = true,
            "--list" => a.list = true,
            "--no-clients" => a.no_clients = true,
            "--no-smooth" => a.no_smooth = true,
            "--no-icons" => a.no_icons = true,
            "--damping" => {
                let v = it.next().ok_or("--damping needs a value in seconds")?;
                a.damping = v
                    .parse::<f64>()
                    .ok()
                    .filter(|d| d.is_finite())
                    .ok_or("--damping must be a finite number of seconds")?;
            }
            "--fps" => {
                let v = it.next().ok_or("--fps needs a value")?;
                a.fps = v.parse::<u64>().map_err(|_| "--fps must be a number")?.clamp(5, 240);
            }
            "-g" | "--focus" => {
                let v = it.next().ok_or("--focus needs a GPU index")?;
                a.focus = Some(v.parse::<usize>().map_err(|_| "--focus must be an index")?);
            }
            other => return Err(format!("unrecognised argument: {}", other)),
        }
    }
    Ok(a)
}

/// Accepts `03:00.0` as shorthand for `0000:03:00.0`, the form the PMU name
/// and sysfs use, and ignores case.
fn normalise_pci(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    if s.matches(':').count() == 1 {
        format!("0000:{}", s)
    } else {
        s
    }
}

/// Renders a string for a `key=value` pair in `--once` output. A single token
/// of printable characters is left bare; anything else is double-quoted, with
/// `\` and `"` escaped and control characters written as `\xNN`.
///
/// Process names are set by whatever runs inside a container. Printed raw, a
/// name could carry terminal escape sequences into root's terminal, and
/// `Plex Transcoder` or `CT 101` would split into two fields for any parser.
/// (The TUI is not exposed: ratatui drops control characters when drawing.)
fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| !c.is_control() && !c.is_whitespace() && c != '"' && c != '\\');
    if plain {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".into())
}

fn build(args: &Args) -> Result<Vec<app::Gpu>, String> {
    let devices = pmu::discover();
    if devices.is_empty() {
        let xe = pmu::cards_on_driver("xe");
        if !xe.is_empty() {
            return Err(format!(
                "no i915 GPU found. {} driven by the xe driver ({}), which igtop does not \
                 support yet: xe publishes a different PMU and fdinfo format.",
                if xe.len() == 1 { "One card is" } else { "Cards are" },
                xe.join(", ")
            ));
        }
        return Err("no i915 PMU found under /sys/bus/event_source/devices \
             (is there an Intel GPU on the i915 driver? `lspci -k` shows the driver)"
            .into());
    }

    let resolved = app::resolve(&devices);

    // A filter that matches nothing is a wrong address, not a permissions
    // problem, and must not fall through to the "needs root" message below.
    if let Some(filter) = &args.device {
        if !resolved.iter().any(|(d, _)| d.pci_addr.as_deref() == Some(filter.as_str())) {
            let found: Vec<String> =
                resolved.iter().filter_map(|(d, _)| d.pci_addr.clone()).collect();
            return Err(format!(
                "no i915 GPU at {} (found: {}). PCI addresses move when cards do; see --list",
                filter,
                if found.is_empty() { "none".into() } else { found.join(", ") }
            ));
        }
    }

    let mut gpus = Vec::new();
    let mut first_err: Option<String> = None;

    for (dev, card) in resolved {
        if let Some(filter) = &args.device {
            if dev.pci_addr.as_deref() != Some(filter.as_str()) {
                continue;
            }
        }
        match app::Gpu::new(&dev, card) {
            Ok(mut g) => {
                g.set_damping(args.damping);
                gpus.push(g)
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(format!("{}: {}", dev.pmu_name, e));
                }
            }
        }
    }

    if gpus.is_empty() {
        let paranoid = std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".into());
        let uid = unsafe { libc::geteuid() };
        return Err(format!(
            "could not open any i915 PMU counters ({})\n\
             euid={}  perf_event_paranoid={}\n\
             This needs root: a system-wide PMU is unreadable without CAP_PERFMON.",
            first_err.unwrap_or_else(|| "no devices matched".into()),
            uid,
            paranoid
        ));
    }
    Ok(gpus)
}

fn main() {
    // Rust ignores SIGPIPE, so `igtop --once | head -1` panicked with "failed
    // printing to stdout: Broken pipe" on the first line head did not want.
    // The default disposition — die quietly, as every Unix filter does — is the
    // right behaviour for the text modes. The TUI owns its terminal and never
    // writes to a pipe, so it is unaffected either way.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("igtop: {}\n\n{}", e, USAGE);
            std::process::exit(2);
        }
    };

    if args.list {
        for d in pmu::discover() {
            println!(
                "{:<24} type={:<4} pci={:<13} engines: {}",
                d.pmu_name,
                d.pmu_type,
                d.pci_addr.clone().unwrap_or_else(|| "-".into()),
                d.engines.iter().map(|e| e.name.clone()).collect::<Vec<_>>().join(" ")
            );
        }
        return;
    }

    let mut gpus = match build(&args) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("igtop: {}", e);
            std::process::exit(1);
        }
    };

    if args.once {
        snapshot(&mut gpus, &args);
        return;
    }

    if let Err(e) = run_tui(&mut gpus, args) {
        ratatui::restore();
        eprintln!("igtop: {}", e);
        std::process::exit(1);
    }
}

/// One-shot text output. Two samples are needed because every headline figure
/// here is a rate, so the interval is genuinely waited out rather than faked.
fn snapshot(gpus: &mut [app::Gpu], args: &Args) {
    for g in gpus.iter_mut() {
        g.tick(!args.no_clients);
    }
    std::thread::sleep(Duration::from_millis(args.interval_ms.max(200)));
    for g in gpus.iter_mut() {
        g.tick(!args.no_clients);
    }

    for g in gpus.iter() {
        let st = g.sysfs.as_ref().map(|s| s.stat.clone()).unwrap_or_default();
        println!(
            "{} [{}]",
            g.name(),
            g.pci_addr.clone().unwrap_or_else(|| "-".into())
        );
        for (i, e) in g.reader.engines.iter().enumerate() {
            println!(
                "  engine {:<6} {:>6.2}%",
                e.name,
                g.sample.engine_busy.get(i).copied().unwrap_or(0.0)
            );
        }
        let na = |o: Option<String>| o.unwrap_or_else(|| "n/a".into());
        println!("  pmu_status        {}", if g.pmu_failed { "read-failed" } else { "ok" });
        println!("  freq_act_mhz      {:.0}", g.sample.actual_freq_mhz);
        println!("  freq_req_mhz      {:.0}", g.sample.requested_freq_mhz);
        println!("  freq_act_avg_mhz  {:.0}", g.sample.actual_freq_avg_mhz);
        println!("  freq_req_avg_mhz  {:.0}", g.sample.requested_freq_avg_mhz);
        println!("  freq_cap_mhz      {}", st.freq_max_mhz);
        println!("  rc6_pct           {:.2}", g.rc6_pct());
        println!("  rc6_pmu_pct       {:.2}", g.sample.rc6_pct);
        println!("  awake_pct         {:.2}", g.sample.awake_pct);
        println!("  irq_per_s         {:.0}", g.sample.interrupts_per_s);
        println!("  power_w           {}", na(g.dynamic.power_w.map(|w| format!("{:.2}", w))));
        println!("  power_max_w       {:.0}", st.power_max_w);
        println!("  temp_c            {}", na(g.dynamic.temp_c.map(|t| format!("{:.1}", t))));
        println!("  vram_used_b       {}", na(g.dynamic.vram_used_bytes.map(|u| u.to_string())));
        println!(
            "  vram_total_b      {}",
            na(Some(st.vram_total_bytes).filter(|t| *t > 0).map(|t| t.to_string()))
        );
        println!("  bar_bytes         {}", st.bar_bytes);
        println!("  link_cur          {}", na(g.dynamic.link_cur.map(|l| l.to_string())));
        println!("  link_max          {}", na(st.link_max.map(|l| l.to_string())));
        println!("  link_dev_max      {}", na(st.link_dev_max.map(|l| l.to_string())));
        println!(
            "  throttle          {}",
            if g.dynamic.throttles.is_empty() {
                "none".to_string()
            } else {
                g.dynamic.throttles.join(",")
            }
        );
        for c in g.clients.iter() {
            let e = |k: &str| c.engines.get(k).copied().unwrap_or(0.0);
            println!(
                "  client pid={} where={} comm={} video={:.1}% enhance={:.1}% render={:.1}% \
                 copy={:.1}% compute={:.1}% mem_b={}",
                c.pid,
                quote(c.container.as_deref().unwrap_or("host")),
                quote(&c.comm),
                e("video"),
                e("video-enhance"),
                e("render"),
                e("copy"),
                e("compute"),
                c.mem_resident,
            );
        }
    }
}

/// Sample, wait, sample again: every headline figure is a rate, so without
/// this the first frame — at start-up or on resuming — is a screen of zeros.
fn prime(gpus: &mut [app::Gpu], scan_clients: bool, interval_ms: u64) {
    for g in gpus.iter_mut() {
        g.tick(scan_clients);
    }
    std::thread::sleep(Duration::from_millis(interval_ms.min(500)));
    for g in gpus.iter_mut() {
        g.tick(scan_clients);
    }
}

fn run_tui(gpus: &mut Vec<app::Gpu>, args: Args) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let host = hostname();
    let mut interval = args.interval_ms;
    let mut show_clients = !args.no_clients;
    let mut show_history = true;
    let mut smooth = !args.no_smooth;
    let mut focus: Option<usize> = args.focus;
    let mut icons = !args.no_icons;
    let mut paused = false;

    prime(gpus, show_clients, interval);

    let mut last_tick = Instant::now();
    // Client rates are held to ~1 Hz no matter how fast the PMU is polled:
    // fdinfo counters move slowly, and a faster scan only adds noise.
    let mut last_client_scan = Instant::now();
    let mut last_frame = Instant::now();
    let frame_budget = Duration::from_millis((1000 / args.fps).max(1));
    // `dirty`: something changed that must reach the screen. `moving`: a bar was
    // still gliding on the previous pass. With neither, the frame would be
    // identical to the last, so it is not drawn at all.
    let mut dirty = true;
    let mut moving = false;
    // What the animated bars showed in the last frame drawn.
    let mut last_sig: Vec<i64> = Vec::new();

    loop {
        let now = Instant::now();
        let tick_every = Duration::from_millis(interval);
        if !paused && now.duration_since(last_tick) >= tick_every {
            let scan = show_clients && now.duration_since(last_client_scan) >= Duration::from_millis(1000);
            if scan {
                last_client_scan = now;
            }
            for g in gpus.iter_mut() {
                g.tick(scan);
            }
            last_tick = now;
            // Every sample is drawn, settled or not: this is what keeps
            // temperature, clock and power current at the sampling rate.
            dirty = true;
        }

        // Animation runs on wall time between frames, independent of both the
        // PMU interval and the frame rate, so the feel is identical whatever
        // either is set to. Coming out of rest, the time since the last frame
        // is the whole idle wait; fed in, it would jump a bar a second's worth
        // in one frame, so motion restarts from one frame's worth instead.
        let frame_dt = if moving { now.duration_since(last_frame) } else { frame_budget };
        last_frame = now;
        moving = false;
        if !paused {
            for g in gpus.iter_mut() {
                // With damping off nothing glides, so nothing may hold the
                // loop at frame rate; otherwise `--no-smooth` would redraw at
                // the frame rate between samples, to no effect.
                if smooth {
                    moving |= g.animate(frame_dt.as_secs_f64());
                } else {
                    g.snap_smooth();
                }
            }
        }

        if dirty || moving {
            let view = ui::View {
                gpus,
                host: host.clone(),
                show_clients,
                show_history,
                smooth,
                icons,
                focus,
                interval_ms: interval,
                paused,
            };
            // A frame that only advances the animation is drawn only if a bar
            // has moved by at least a displayable step; see `motion_signature`.
            let sig = ui::motion_signature(&view, terminal.size()?.width);
            if dirty || sig != last_sig {
                terminal.draw(|f| ui::draw(f, &view))?;
                last_sig = sig;
            }
            dirty = false;
        }

        // While a bar is gliding, wake at the frame rate. Otherwise nothing can
        // change before the next sample or a key, so sleep until the sample is
        // due — and while paused, until a key arrives.
        let wait = if moving {
            frame_budget
        } else if paused {
            Duration::from_secs(1)
        } else {
            (last_tick + tick_every)
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1))
        };
        if !event::poll(wait)? {
            continue;
        }
        match event::read()? {
            Event::Resize(..) => dirty = true,
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                dirty = true;
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('c') => {
                        show_clients = !show_clients;
                        // Hidden means not scanned, so the old baseline spans
                        // the whole time it was hidden. Start afresh.
                        if show_clients {
                            for g in gpus.iter_mut() {
                                g.reset_clients();
                            }
                            last_client_scan = Instant::now();
                        }
                    }
                    KeyCode::Char('h') => show_history = !show_history,
                    KeyCode::Char('s') => smooth = !smooth,
                    KeyCode::Char('i') => icons = !icons,
                    // all -> first -> second -> ... -> all
                    KeyCode::Char('g') => {
                        let n = gpus.len();
                        focus = match focus {
                            _ if n < 2 => None,
                            None => Some(0),
                            Some(i) if i + 1 < n => Some(i + 1),
                            _ => None,
                        };
                    }
                    KeyCode::Char(' ') => {
                        paused = !paused;
                        // Every figure is a rate over the time since the last
                        // sample. Resumed as-is, the first one would average
                        // the whole pause and show it as the current reading.
                        if !paused {
                            for g in gpus.iter_mut() {
                                g.reset_rates();
                            }
                            prime(gpus, show_clients, interval);
                            last_tick = Instant::now();
                            last_client_scan = last_tick;
                        }
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        interval =
                            INTERVAL_STEPS.iter().rev().copied().find(|&s| s < interval).unwrap_or(interval);
                    }
                    KeyCode::Char('-') | KeyCode::Char('_') => {
                        interval = INTERVAL_STEPS.iter().copied().find(|&s| s > interval).unwrap_or(interval);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    ratatui::restore();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_shorthand() {
        assert_eq!(normalise_pci("03:00.0"), "0000:03:00.0");
        assert_eq!(normalise_pci(" 0000:0A:00.0 "), "0000:0a:00.0");
    }

    #[test]
    fn once_quoting() {
        assert_eq!(quote("ffmpeg"), "ffmpeg");
        assert_eq!(quote("CT 101"), "\"CT 101\"");
        assert_eq!(quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(quote("x\x1b[2J"), "\"x\\x1b[2J\"");
        assert_eq!(quote(""), "\"\"");
    }
}
