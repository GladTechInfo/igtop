//! Minimal `perf_event_open` binding.
//!
//! The `libc` crate's `perf_event_attr` is feature-gated and has shifted shape
//! between releases, so the struct is declared here instead. `size` is set from
//! `size_of`, which is how the kernel negotiates version compatibility.

use std::io;
use std::os::unix::io::RawFd;

pub const PERF_FORMAT_TOTAL_TIME_ENABLED: u64 = 1 << 0;
pub const PERF_FORMAT_TOTAL_TIME_RUNNING: u64 = 1 << 1;
pub const PERF_FORMAT_ID: u64 = 1 << 2;
pub const PERF_FORMAT_GROUP: u64 = 1 << 3;

const PERF_FLAG_FD_CLOEXEC: u64 = 1 << 3;

/// The request type differs by libc: `c_ulong` on glibc, `c_int` on musl,
/// so `libc::Ioctl` is used rather than a fixed width. The value exceeds
/// `i32::MAX`, hence the explicit cast to preserve the bit pattern either way.
/// `_IOR('$', 7, __u64)` — retrieves the unique id the kernel assigned an event,
/// which is what lets a grouped read be mapped back to its counter reliably
/// rather than by assuming sibling ordering.
const PERF_EVENT_IOC_ID: libc::Ioctl = 0x8008_2407u32 as libc::Ioctl;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PerfEventAttr {
    pub type_: u32,
    pub size: u32,
    pub config: u64,
    pub sample_period: u64,
    pub sample_type: u64,
    pub read_format: u64,
    pub flags: u64,
    pub wakeup_events: u32,
    pub bp_type: u32,
    pub config1: u64,
    pub config2: u64,
    pub branch_sample_type: u64,
    pub sample_regs_user: u64,
    pub sample_stack_user: u32,
    pub clockid: i32,
    pub sample_regs_intr: u64,
    pub aux_watermark: u32,
    pub sample_max_stack: u16,
    pub reserved_2: u16,
    pub aux_sample_size: u32,
    pub reserved_3: u32,
    pub sig_data: u64,
    pub config3: u64,
}

impl Default for PerfEventAttr {
    fn default() -> Self {
        // Safe: the struct is plain old data with no padding invariants and no
        // niche-bearing types, so an all-zero bit pattern is a valid value.
        let mut a: Self = unsafe { std::mem::zeroed() };
        a.size = std::mem::size_of::<Self>() as u32;
        a
    }
}

unsafe fn perf_event_open(
    attr: &PerfEventAttr,
    pid: libc::pid_t,
    cpu: i32,
    group_fd: RawFd,
    flags: u64,
) -> io::Result<RawFd> {
    let ret = libc::syscall(
        libc::SYS_perf_event_open,
        attr as *const PerfEventAttr,
        pid,
        cpu,
        group_fd as libc::c_int,
        flags,
    );
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret as RawFd)
    }
}

/// A group of counters opened against one PMU, read atomically in a single
/// `read(2)` so every counter in a sample shares an instant.
pub struct PerfGroup {
    fds: Vec<RawFd>,
    ids: Vec<u64>,
    buf: Vec<u64>,
}

impl PerfGroup {
    pub fn new() -> Self {
        PerfGroup { fds: Vec::new(), ids: Vec::new(), buf: Vec::new() }
    }

    /// Adds one counter. The first successful add becomes the group leader; the
    /// rest are opened against it so they are scheduled together.
    pub fn add(&mut self, pmu_type: u32, config: u64, cpu: i32) -> io::Result<()> {
        let attr = PerfEventAttr {
            type_: pmu_type,
            config,
            read_format: PERF_FORMAT_GROUP
                | PERF_FORMAT_ID
                | PERF_FORMAT_TOTAL_TIME_ENABLED
                | PERF_FORMAT_TOTAL_TIME_RUNNING,
            ..PerfEventAttr::default()
        };
        // No exclude_* bits: the i915 PMU sets PERF_PMU_CAP_NO_EXCLUDE and
        // rejects any of them outright.

        let leader = self.fds.first().copied().unwrap_or(-1);

        // pid = -1 means system-wide, which is the only mode this PMU supports
        // and is also what lets a transcode inside a container be counted from
        // the host without touching the container's namespace.
        let fd = unsafe { perf_event_open(&attr, -1, cpu, leader, PERF_FLAG_FD_CLOEXEC)? };

        let mut id: u64 = 0;
        let rc = unsafe { libc::ioctl(fd, PERF_EVENT_IOC_ID, &mut id as *mut u64) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }

        self.fds.push(fd);
        self.ids.push(id);
        self.buf.resize(3 + 2 * self.fds.len(), 0);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.fds.len()
    }

    /// Reads every counter in the group. Returns the group's `time_enabled` in
    /// nanoseconds, and values indexed to match the order `add` was called in,
    /// having mapped through the kernel-assigned ids.
    ///
    /// `time_enabled` is the kernel's own clock, captured in the same read as
    /// the counters, so it is the right denominator for a rate: a timestamp
    /// taken in userspace before or after the read is off by however long the
    /// process was delayed in between. intel_gpu_top uses it the same way.
    ///
    /// Values are corrected for multiplexing via `enabled`/`running`. That ratio
    /// is 1.0 for this PMU in practice, but a counter scaled by a stale ratio is
    /// a silent wrong number, so the correction is applied unconditionally.
    pub fn read(&mut self) -> io::Result<(u64, Vec<u64>)> {
        if self.fds.is_empty() {
            return Ok((0, Vec::new()));
        }
        let want = self.buf.len() * 8;
        let n = unsafe {
            libc::read(self.fds[0], self.buf.as_mut_ptr() as *mut libc::c_void, want)
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if (n as usize) < want {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short PMU group read"));
        }

        let nr = self.buf[0] as usize;
        let enabled = self.buf[1];
        let running = self.buf[2];
        let scale = if running > 0 && enabled > running {
            enabled as f64 / running as f64
        } else {
            1.0
        };

        let mut out = vec![0u64; self.fds.len()];
        for i in 0..nr.min(self.fds.len()) {
            let value = self.buf[3 + i * 2];
            let id = self.buf[3 + i * 2 + 1];
            if let Some(slot) = self.ids.iter().position(|&x| x == id) {
                out[slot] = if scale > 1.0 {
                    (value as f64 * scale) as u64
                } else {
                    value
                };
            }
        }
        Ok((enabled, out))
    }
}

impl Drop for PerfGroup {
    fn drop(&mut self) {
        for &fd in &self.fds {
            unsafe { libc::close(fd) };
        }
    }
}
