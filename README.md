# igtop

A terminal GPU monitor for **Intel** cards on the `i915` driver and **AMD**
cards on `amdgpu`, that reads the kernel's own counters instead of guessing. A
machine with cards of both makes shows them all in one instance.

![igtop monitoring an Arc A380 during a hardware transcode](docs/screenshot.png)

![igtop monitoring a Radeon RX 6950 XT through a VA-API transcode: real time, a full-speed burst, then a 4K HEVC pass](docs/screenshot-amd.png)

- **Per-engine utilisation** as true time ratios: every video, video-enhance,
  render, copy and compute engine on Intel; decode, encode, JPEG, GFX, DMA and
  compute on AMD.
- **The clock the GPU actually ran at**, not the requested cap.
- **Per-process attribution**, labelled with the container or VM the process
  runs in (Proxmox LXC and QEMU, Docker, Podman, systemd-nspawn).
- **Temperature, power, VRAM, PCIe link and throttle reasons** where the card
  exposes them.
- **`--once`** for a plain-text snapshot that scripts can parse.

It was written for watching hardware transcoding (Plex, Jellyfin, ffmpeg), so
the media engines are the headline, but it works for any workload.

igtop is an independent project, unaffiliated with Intel or AMD, and is not
part of Intel's `igt-gpu-tools`.

## Why not nvtop, intel_gpu_top or amdgpu_top?

On Intel, nvtop misreads Arc cards in several ways at the time of writing:

| Reading       | nvtop                         | igtop                                       |
|---------------|-------------------------------|----------------------------------------------|
| Utilisation   | none (`gpu_util: null`)       | real per-engine %, from the i915 PMU         |
| Clock         | the requested cap (constant)  | the achieved clock, while awake              |
| Video engines | not broken out                | `vcs0` `vcs1` `vecs0` `vecs1` separately     |
| Attribution   | PID only                      | PID plus its container, e.g. `CT 101`        |
| Throttling    | not shown                     | each `throttle_reason_*` flag                |
| rc6 idle      | not shown                     | live residency                               |
| Scripting     | interactive only              | `--once` emits plain key/value text          |

The clock is the important one. nvtop reads `gt_cur_freq_mhz`, which is the
*requested* frequency, not the achieved clock: an idle card reads its maximum
while the hardware is asleep.

`intel_gpu_top` reads the same PMU and is accurate. igtop adds per-container
attribution, the sysfs sensors, a history graph, and a clock figure that is not
diluted by idle time (see [Intel clock](#intel-clock)).

On AMD, nvtop and `amdgpu_top` are mature and accurate, and `amdgpu_top` shows
far more of the card than igtop does. igtop's case there is narrower: the same
view and the same `--once` output for every card in a mixed Intel and AMD
machine, container labels on each process, and the media engines broken out as
the headline.

## Supported hardware

Check which driver each card is on with:

    lspci -k | grep -A3 -E 'VGA|Display'     # "Kernel driver in use: i915" or "amdgpu"

What is shown depends on what the driver exposes. Missing readings are left
out, not shown as zero.

### Intel, on `i915`

Arc Alchemist (A310, A380, A580, A750, A770 and the mobile parts) and
integrated graphics from Gen9 onward (Skylake to Alder Lake, Raptor Lake and
Meteor Lake on most distributions).

| | Discrete (Arc) | Integrated |
|---|---|---|
| Engine utilisation, clock, rc6, throttle reasons, clients | yes | yes |
| Temperature, power | yes | no — i915 publishes no hwmon for iGPUs |
| VRAM | yes | no — shared system memory |
| PCIe link | yes | no — not on a PCIe link |

**Not supported: the `xe` driver.** Arc Battlemage (B570, B580), Lunar Lake,
Panther Lake and later are `xe`-only, and Alchemist cards can optionally be
bound to `xe` as well. `xe` publishes a different PMU and fdinfo format, so
igtop finds nothing there and says so.

### AMD, on `amdgpu`

Discrete Radeon cards and integrated graphics (APUs). amdgpu publishes no PMU,
so the engines are measured differently; see [AMD engines](#amd-engines).

| | Discrete | Integrated (APU) |
|---|---|---|
| Engine utilisation, clock, firmware load, clients | yes | yes |
| Temperature | junction, against the card's own critical limit | edge |
| VRAM | yes | yes — the carve-out reserved for the GPU |
| Power, PCIe link | yes | no — an APU's figures are the whole package's |
| Throttle reasons | not yet | not yet |

Developed and tested on an Arc A380 under Linux 7.0, on a UHD Graphics 630, and
on a Radeon RX 6950 XT and an integrated Radeon 610M under Linux 7.3. Reports
from other cards are welcome.

## Requirements

- Linux on x86-64. Per-client figures, and on AMD the engine bars too, need
  the kernel's standard fdinfo engine figures: Linux 5.19 or later, and on
  Intel 6.5 or later for parts using GuC submission (Arc, and some recent
  integrated GPUs). On Intel the rest works without them.
- **Root**, or `CAP_PERFMON`, for Intel cards. The i915 PMU is a system-wide
  event source, and with the usual `kernel.perf_event_paranoid` setting an
  unprivileged process cannot open it. Lowering that sysctl affects every
  process on the machine; running one tool as root is the smaller trade.
- **Root is recommended for AMD cards.** They work without it, but then count
  only the user's own processes, and the display says so. On a mixed machine
  run without root, the Intel card is reported as not shown rather than
  dropped silently.
- `debugfs` mounted at `/sys/kernel/debug` for Intel VRAM figures and the fast
  client scan. Without it, Intel VRAM is omitted and clients are found by
  walking `/proc`. AMD VRAM comes from sysfs and needs no debugfs.
- A [Nerd Font](https://www.nerdfonts.com/) for the icons, or `--no-icons`.

## Installing

Each [release](https://github.com/lukazibi83/igtop/releases/latest) carries
ready-made builds, checked against `SHA256SUMS`:

| File | For |
|---|---|
| `igtop_<version>-1_amd64.deb` | Debian, Ubuntu, Proxmox: `sudo apt install ./igtop_*.deb` |
| `igtop-<version>-1.x86_64.rpm` | Fedora, RHEL, Rocky, Alma, openSUSE: `sudo dnf install ./igtop-*.rpm` (or `zypper install`) |
| `igtop-<version>-x86_64-linux` | Any x86-64 Linux: a static binary, no dependencies; `chmod +x` and run |

All three contain the same static binary, so they behave identically on any
distribution. Each is installed and run in containers of the distributions it
is for before a release is published.

**Arch Linux and derivatives:** an AUR package is on its way (AUR account
registration is paused at the time of writing). Until then, build the same
package from this repository:

    git clone https://github.com/lukazibi83/igtop
    cd igtop/packaging/aur && makepkg -si

## Building

With a Rust toolchain ([rustup](https://rustup.rs/)):

    cargo build --release
    sudo ./target/release/igtop

For a **static binary** to copy to other machines — a hypervisor, say, where
you would rather not install a compiler:

    rustup target add x86_64-unknown-linux-musl
    ./build.sh
    # → target/x86_64-unknown-linux-musl/release/igtop

`build.sh` targets baseline x86-64, so the binary runs on any x86-64 CPU, and
refuses the result if it contains AVX-512 instructions. Some distributions
(CachyOS, for one) ship a Rust standard library built for x86-64-v4, which puts
AVX-512 in every binary regardless of `-C target-cpu`; such a binary dies with
`Illegal instruction` on older CPUs. If `build.sh` reports this, rebuild the
standard library from source:

    rustup component add rust-src
    IGTOP_BUILD_STD=1 ./build.sh

Tests:

    cargo test
    cargo test --release -- --ignored --nocapture bench    # drawing benchmark

## Usage

    sudo igtop                 # TUI: 1 s sampling, 30 fps animation
    sudo igtop --once          # one plain-text snapshot, for scripts
    sudo igtop --list          # list detected GPUs and exit
    sudo igtop -d 03:00.0      # one card only; short or full (0000:03:00.0) form
    sudo igtop --no-smooth     # raw, undamped values
    sudo igtop --damping 6     # seconds for a bar to fall ~90% (default 4)
    sudo igtop --fps 60        # smoother animation, at more CPU
    sudo igtop --no-icons      # ASCII markers if the font lacks Nerd Font glyphs

`igtop --help` lists every option.

**Keys:** `q` quit · `c` client table · `h` history · `s` damping on/off ·
`g` cycle GPU (all → first → second → … → all) · `i` icons · `space` pause ·
`+`/`-` refresh rate (50, 100, 250, 500, 1000, 2000, 5000, 10000 ms).

Resuming from a pause, or showing the client table again, starts every rate
afresh, so the first reading never averages the gap.

## The display

Each card has its own panel, Intel and AMD alike: a status line, a **MEDIA**
headline — the busiest video engine (Intel's video and video-enhance; AMD's
decode, encode and JPEG) — over one bar per engine, then **TEMP, POWER, CLOCK,
MEMORY**, each against its ceiling. Below that a history graph and the client
table.

The **status line** shows rc6, awake time and interrupts on Intel, and the
firmware's **gfx load** and **mem load** on AMD; then the PCIe link, the fan
and any throttle reasons.

**History** is one column per second, the mean over that second, whatever the
refresh rate; ten minutes are kept. The newest second is at the right edge. The
graph follows the engine with the highest peak on screen.

**PCIe link** is judged against what the slot can train to — the lower of the
card's own maximum and the root port's — so a Gen4 card in a Gen3 slot at Gen3
is shown as normal. Only a genuine shortfall turns red. Arc and recent Radeon
boards carry an internal PCIe switch, so the link reported is the card's upstream
port, not the GPU function's meaningless internal one.

**TEMP** on AMD is the junction (hotspot) sensor against the card's own
critical limit. Intel publishes no maximum temperature for Arc cards and the
driver exposes none, so on Intel TEMP is scaled to 90 °C: a display scale, not
a vendor limit; the authoritative overheating signal is the `THERMAL` throttle
reason.

**Damping.** Bars rise fast and fall slowly, like a shock absorber, so a spike
is never missed but the display does not flicker. The time constants are in
seconds, so the feel does not change with the refresh rate. Damping is display
only: `--once`, the figures beside the bars and the raw history are untouched.
Press `s` for raw bars.

If an Intel PMU read fails, the GPU's title says **PMU read failed — figures
frozen** rather than leaving stale figures looking live. An AMD card read
without root says **not root — own processes only**.

## `--once` format

GPU figures are `key value` lines, the value running to end of line. A
`driver` line says which driver the card is on; the keys that follow are that
driver's (`rc6_pct` and `freq_req_mhz` on Intel, `gpu_busy_pct` and
`engine_scope` on AMD, for example). Each client is a line of `key=value`
pairs, its engine keys named as the driver names them (`video`, `render`, …;
`dec`, `gfx`, …).

A value containing spaces, quotes or control characters is double-quoted with
`\\`, `\"` and `\xNN` escapes — for example `where="CT 101" comm="Plex
Transcoder"`. Process names are set by whatever runs in a container, so they
are never printed raw. Piping into `head` exits quietly.

## How the figures are measured

### Intel engines

`/sys/bus/event_source/devices/i915*` exposes a busy-time counter in
nanoseconds per engine, so utilisation is a true time ratio, not a sampled
guess. Rates divide by the kernel's own `time_enabled`, read atomically with the
counters. The PMU is system-wide, so work inside a container is counted from
the host without entering it.

### AMD engines

amdgpu has no system-wide busy counter. What it publishes is each client's
cumulative time per engine class in `/proc/<pid>/fdinfo`, so igtop adds up the
advance of every client on the card over each interval. That is still a time
ratio, and work inside a container is still counted from the host, but it
differs from i915's PMU in three ways:

- A job counts from when it reaches the hardware ring until it completes, so
  work queued behind another context's counts as busy. Several contexts on one
  class can add up to more than wall time; the bar stops at 100 %.
- Work by a client that exits mid-interval is lost, and a client is measured
  from its first sighting, so very short-lived clients are undercounted.
- Only processes igtop may inspect are counted: all of them as root.

The firmware's own `gpu_busy_percent` and `mem_busy_percent` are shown on the
status line as **gfx load** and **mem load**. They cover the graphics block and
the memory controller only: during a hardware decode at 86 % the firmware's
figure read under 10 %, which is why it is not used for the bars.

### Intel clock

The kernel integrates **0 MHz while the GPU is parked**, so the plain mean over
an interval is the running clock multiplied by the fraction of time awake: a
GPU at 2000 MHz for 30 % of a second averages ~600. That diluted figure is what
`intel_gpu_top` shows. igtop's CLOCK divides the same integral by the PMU's
awake time instead, giving the clock the GPU actually ran at. `--once` prints
both: `freq_act_mhz` (while awake) and `freq_act_avg_mhz` (diluted).

### AMD clock

hwmon's `sclk` input: the shader clock the card is running at the moment of
sampling, which amdgpu reports as achieved, not requested. An idle card reads
tens of MHz, not its ceiling. The ceiling is the top level of `pp_dpm_sclk`.

### rc6 (Intel)

rc6 residency is read from sysfs (`rc6_residency_ms`), the hardware counter,
not from the PMU. With GuC SLPC the PMU's `rc6-residency` event can stand still
on an idle card while the hardware counter advances: `get_rc6()` in
`i915_pmu.c` never reports less than it last did, so an estimate that once ran
ahead of the hardware holds the event flat until the hardware catches up.
`--once` prints both as `rc6_pct` and `rc6_pmu_pct`.

### Clients

Per-process figures come from `/proc/<pid>/fdinfo` on both drivers,
deduplicated on `(drm-pdev, drm-client-id)` — a client holding several
descriptors reports the same cumulative counters on each, so summing multiplies
the answer. Several clients in one process (a transcoder using VA-API to decode
and OpenCL to tonemap, say) are folded into one row. The processes to examine
come from the DRM core's own list in debugfs; walking every process's file
descriptors is the fallback, and on a busy host costs far more. On AMD the
same scan feeds the engine bars, so processes are walked once per sample.

The table's columns follow the driver: VIDEO, ENHANCE, RENDER and COMPUTE on
Intel; DECODE, ENCODE, GFX and COMPUTE on AMD.

⚠ **Client figures lag the engine bars on Intel.** With GuC submission, i915
updates a context's runtime when the context is switched out, so over a short
window a busy client reads in bursts. The table therefore averages each client
over 10 seconds, on both drivers. The engine bars are the live measure; the
table is attribution. Its figures are engine-*class* averages: with two video
engines, a client saturating one reads 50 % against that engine's 100 % bar.

**Client memory** is an estimate. Both drivers count a buffer in the resident
figure of every client holding a handle to it, and in its shared figure too. A
process's row adds each client's private memory and counts shared memory once,
as the largest client's shared figure. Which buffers overlap is not published.

### Cost

Drawing is nearly all of igtop's CPU cost: about 2.5 % of one core at the
default 30 fps during a transcode, less when idle, since a frame is drawn only
when a bar would move by a visible step. `--fps` trades smoothness for CPU;
`--no-smooth` draws only on each sample.

## Limitations

- `i915` and `amdgpu` only; see [Supported hardware](#supported-hardware).
- AMD throttle reasons are not shown yet: amdgpu publishes them only inside the
  binary `gpu_metrics` table, whose layout varies by chip.
- On Intel parts with a separate media GT (Meteor Lake), CLOCK and rc6
  describe the primary GT only. The media engines' utilisation is still shown.
- Container labels cover Proxmox, Docker, Podman and systemd-nspawn. Other
  runtimes show as `host`.

## Licence

GNU General Public License v3.0 or later. See [LICENSE](LICENSE).
