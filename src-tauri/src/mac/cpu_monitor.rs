//! Self-accounting CPU/power monitor, for diagnosing fan complaints from a
//! field log instead of a screen-share.
//!
//! The fan question is really two questions the kernel already answers per
//! process, no root required:
//!
//! * **Where did the CPU time run?** `proc_pid_rusage(RUSAGE_INFO_V4)` splits
//!   this task's CPU time by QoS class. On Apple Silicon class ≈ cluster:
//!   `background`/`maintenance` time ran on the E-cluster (cannot spin a fan);
//!   every other class runs on performance cores at full clock. The regression
//!   that heats an M2 Pro is heavy time *outside* the background band — the
//!   0.5.3 bug showed up as 100 CPU-seconds of `utility`.
//! * **How much power was that?** `task_info(TASK_POWER_INFO_V2).task_energy`
//!   is the energy (nJ) the scheduler billed to this task. ΔJ/Δs is average
//!   watts — the quantity fans actually respond to. Measured reference points
//!   on the fresh-index workload, which is what the thresholds below are set
//!   against: healthy walk ≈ 2W, single-writer finalize ≈ 6W, the all-`UTILITY`
//!   0.5.3 bug ≈ 11W, the pre-0.5.2 inherited-QoS bug ≈ 17W. An M-series MBP
//!   needs ~20W sustained before the fan is audible. (Intel Macs don't populate
//!   `task_energy`; the watchdog falls back to a P-band core-count threshold
//!   there, which also covers a `task_info` that stops answering.)
//!
//! ## The per-class billing follows *effective* QoS, boosts included
//!
//! `ri_cpu_time_qos_*` bills each quantum to the class the thread was
//! *scheduled at*, not the class it requested. Verified empirically: a
//! `BACKGROUND` thread that another thread is `pthread_join`-ing bills 100% to
//! the joiner's band (turnstile priority donation), while the same burn with
//! nobody blocked on it bills to `background`. Two consequences:
//!
//! * For diagnosis this is the more truthful signal — it reports where the
//!   scheduler actually ran the work, which is what heats the machine. A
//!   `pthread_get_qos_class_np` readback can say `background` while the CPU
//!   time truthfully bills to a boosted band.
//! * The indexer's hot paths are safe from this skew: nothing joins the walk
//!   workers mid-work (the writer waits on a channel, and the scope join
//!   happens after the workers finish), so their time bills `background` as
//!   intended — confirmed end-to-end on a full `$HOME` walk.
//!
//! Two consumers:
//! * [`start_watchdog`] — a resident sampler (GUI + daemon) that logs an
//!   episode into the diag log when the process burns fan-relevant power:
//!   one line at onset with the per-class split, sparse heartbeats while it
//!   lasts, one summary line when it ends. Quiet otherwise, per the diag-log
//!   discipline (a healthy machine logs nothing; a fresh index logs one short
//!   episode for the finalize tail, which doubles as a baseline).
//! * [`Snapshot`] + [`pass_summary`] — pass-boundary accounting appended to
//!   the existing "index pass ok/FAILED" and finalize diag lines, so every
//!   pass records what it cost even when it never crossed the watchdog bar.

use std::time::{Duration, Instant};

use crate::diag;

// `libc` declares proc_pid_rusage/rusage_info_v4/task_info, so the kernel struct
// layouts are its problem rather than ours. Its `mach_timebase_info` and
// `mach_task_self` are #[deprecated] in favour of the mach2 crate, which this
// tree does not depend on — hence local declarations for exactly those two.
extern "C" {
    fn mach_timebase_info(info: *mut [u32; 2]) -> std::os::raw::c_int;
    fn mach_task_self() -> u32;
}

/// QoS-class slots in a delta/snapshot, in `rusage_info_v4` field order.
const QOS_CLASSES: usize = 7;
const QOS_MAINTENANCE: usize = 1;
const QOS_BACKGROUND: usize = 2;
/// Short labels for log lines, indexed like the slots: default, maintenance,
/// background, utility, legacy (the default band pre-QoS), user-initiated,
/// user-interactive.
const QOS_LABELS: [&str; QOS_CLASSES] = ["def", "maint", "bg", "util", "legacy", "uinit", "ui"];

/// Slot index for a label, for tests and callers that would otherwise hardcode
/// a bare number next to a comment naming the class.
#[cfg(test)]
fn qos_slot(label: &str) -> usize {
    QOS_LABELS
        .iter()
        .position(|l| *l == label)
        .unwrap_or_else(|| panic!("unknown QoS label {label:?}"))
}

fn mach_ticks_to_secs(ticks: u64) -> f64 {
    // numer/denom are constants for the machine; cache after the first read.
    static TIMEBASE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    let (numer, denom) = *TIMEBASE.get_or_init(|| {
        let mut info = [0u32; 2];
        if unsafe { mach_timebase_info(&mut info) } != 0 || info[1] == 0 {
            (1, 1)
        } else {
            (info[0], info[1])
        }
    });
    ticks as f64 * numer as f64 / denom as f64 / 1e9
}

/// `task_energy` in joules — 0.0 where the kernel doesn't account it (Intel).
///
/// `TASK_POWER_INFO_V2` has no libc binding, so this one keeps a hand-described
/// layout: `task_power_info` (6 × u64) + `gpu_energy` (4 × u64) + `task_energy`.
fn task_energy_joules() -> f64 {
    const TASK_POWER_INFO_V2: u32 = 26;
    const TASK_ENERGY_SLOT: usize = 10;
    let mut buf = [0u64; 16];
    // `count` is in natural_t (u32) units, hence the u64→u32 word conversion.
    let mut count: u32 = (12 * std::mem::size_of::<u64>() / 4) as u32;
    let kr = unsafe {
        libc::task_info(
            mach_task_self(),
            TASK_POWER_INFO_V2,
            buf.as_mut_ptr() as *mut i32,
            &mut count,
        )
    };
    if kr != 0 {
        return 0.0;
    }
    buf[TASK_ENERGY_SLOT] as f64 / 1e9
}

/// Point-in-time reading of this process's cumulative CPU/energy accounting.
pub(crate) struct Snapshot {
    at: Instant,
    /// Cumulative CPU seconds per QoS class (rusage_info_v4 order).
    qos_secs: [f64; QOS_CLASSES],
    energy_j: f64,
}

impl Snapshot {
    pub(crate) fn take() -> Option<Self> {
        let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::proc_pid_rusage(
                std::process::id() as libc::c_int,
                libc::RUSAGE_INFO_V4,
                &mut ri as *mut _ as *mut libc::rusage_info_t,
            )
        };
        if rc != 0 {
            return None;
        }
        // Per-class CPU time in mach ticks. Order must match QOS_LABELS.
        let qos_secs = [
            ri.ri_cpu_time_qos_default,
            ri.ri_cpu_time_qos_maintenance,
            ri.ri_cpu_time_qos_background,
            ri.ri_cpu_time_qos_utility,
            ri.ri_cpu_time_qos_legacy,
            ri.ri_cpu_time_qos_user_initiated,
            ri.ri_cpu_time_qos_user_interactive,
        ]
        .map(mach_ticks_to_secs);
        Some(Self {
            at: Instant::now(),
            qos_secs,
            energy_j: task_energy_joules(),
        })
    }

    pub(crate) fn delta(&self, later: &Snapshot) -> Delta {
        let mut qos_secs = [0.0; QOS_CLASSES];
        for i in 0..QOS_CLASSES {
            qos_secs[i] = (later.qos_secs[i] - self.qos_secs[i]).max(0.0);
        }
        Delta {
            wall_secs: later.at.duration_since(self.at).as_secs_f64(),
            qos_secs,
            energy_j: (later.energy_j - self.energy_j).max(0.0),
        }
    }
}

/// CPU/energy spent over one observation window.
#[derive(Clone, Copy, Default)]
pub(crate) struct Delta {
    wall_secs: f64,
    qos_secs: [f64; QOS_CLASSES],
    energy_j: f64,
}

impl Delta {
    fn cpu_secs(&self) -> f64 {
        self.qos_secs.iter().sum()
    }

    fn watts(&self) -> f64 {
        if self.wall_secs > 0.0 {
            self.energy_j / self.wall_secs
        } else {
            0.0
        }
    }

    /// Average concurrent cores OUTSIDE the E-cluster band — the ones that can
    /// heat the machine. The Intel fallback signal.
    fn p_band_cores(&self) -> f64 {
        if self.wall_secs <= 0.0 {
            return 0.0;
        }
        let e_band = self.qos_secs[QOS_BACKGROUND] + self.qos_secs[QOS_MAINTENANCE];
        (self.cpu_secs() - e_band) / self.wall_secs
    }

    fn accumulate(&mut self, other: &Delta) {
        self.wall_secs += other.wall_secs;
        for i in 0..QOS_CLASSES {
            self.qos_secs[i] += other.qos_secs[i];
        }
        self.energy_j += other.energy_j;
    }

    /// Per-class parts for a log line: `bg=290% util=98%` as occupancy
    /// ([`Unit::Rate`], 100% = one core) or `bg=74.3s util=18.0s` as consumed
    /// time ([`Unit::Secs`]). Classes below the unit's noise floor are omitted.
    fn class_parts(&self, unit: Unit) -> String {
        QOS_LABELS
            .iter()
            .enumerate()
            .filter_map(|(i, label)| unit.render(self, self.qos_secs[i]).map(|v| format!("{label}={v}")))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// ` 143.7J avg 3.5W` / ` 11.0W` — omitted where the kernel doesn't account
    /// energy (Intel), so a log line never claims 0W.
    fn energy_suffix(&self, with_total: bool) -> String {
        if self.energy_j <= 0.0 {
            return String::new();
        }
        if with_total {
            format!(" {:.1}J avg {:.1}W", self.energy_j, self.watts())
        } else {
            format!(" {:.1}W", self.watts())
        }
    }

    /// One sample window: `cpu=545% (util=350% legacy=180%) 11.2W`.
    fn describe(&self) -> String {
        format!(
            "cpu={:.0}% ({}){}",
            Unit::Rate.scale(self, self.cpu_secs()),
            self.class_parts(Unit::Rate),
            self.energy_suffix(false),
        )
    }

    /// A whole span: `92.3s (bg=74.3s util=18.0s) 143.7J avg 3.5W`.
    fn totals(&self) -> String {
        format!(
            "{:.1}s ({}){}",
            self.cpu_secs(),
            self.class_parts(Unit::Secs),
            self.energy_suffix(true),
        )
    }
}

/// How a per-class CPU figure is presented: as a rate (share of wall time) or
/// as the seconds actually consumed.
#[derive(Clone, Copy)]
enum Unit {
    Rate,
    Secs,
}

impl Unit {
    fn scale(self, d: &Delta, secs: f64) -> f64 {
        match self {
            Unit::Rate if d.wall_secs > 0.0 => secs / d.wall_secs * 100.0,
            Unit::Rate => 0.0,
            Unit::Secs => secs,
        }
    }

    /// The formatted value, or `None` when it is below the noise floor —
    /// a class contributing under 5% of a window, or under half a second of a
    /// span, is not worth a slot in the line.
    fn render(self, d: &Delta, secs: f64) -> Option<String> {
        let value = self.scale(d, secs);
        match self {
            Unit::Rate => (value >= 5.0).then(|| format!("{value:.0}%")),
            Unit::Secs => (value >= 0.5).then(|| format!("{value:.1}s")),
        }
    }
}

/// ` | cpu 92.3s (bg=74.3s util=18.0s) 143.7J avg 3.5W` — appended to the
/// pass-boundary diag lines. Empty when the start snapshot was unavailable.
pub(crate) fn pass_summary(start: Option<Snapshot>) -> String {
    let Some(start) = start else {
        return String::new();
    };
    let Some(now) = Snapshot::take() else {
        return String::new();
    };
    format!(" | cpu {}", start.delta(&now).totals())
}

// -- Watchdog thresholds ------------------------------------------------------

/// Sustained watts that make an episode worth logging. Well under the ~20W
/// fan territory and above the ~2W healthy walk, so onset is recorded long
/// before the user hears anything; the ~6W single-writer finalize crosses it
/// by design, giving every fresh install one baseline episode in the log.
const HOT_WATTS: f64 = 5.0;
/// Intel fallback (no energy accounting): sustained cores outside the
/// background band. 2 cores of P-band work ≈ the same "worth recording" bar.
const HOT_P_BAND_CORES: f64 = 2.0;
/// Consecutive cool samples that end an episode — one quiet window between
/// two hot ones is the same episode, not two.
const COOL_SAMPLES_TO_END: u32 = 2;
/// Heartbeat spacing while an episode lasts: every 30s early on, then backed
/// off hard so a stuck-hot process cannot flood the 5MB log. Spacing is derived
/// from the episode's own elapsed time, so no heartbeat counter is needed.
const HEARTBEAT_EARLY_SECS: f64 = 30.0;
const HEARTBEAT_LATE_SECS: f64 = 300.0;
const HEARTBEAT_BACKOFF_AFTER_SECS: f64 = 150.0;

/// Episode state machine, pure so the thresholds are testable: feed it one
/// [`Delta`] per sample window, log whatever line it returns.
///
/// `episode: Option<Delta>` is the whole "are we in an episode" state — a
/// separate flag could disagree with it.
pub(crate) struct Watchdog {
    episode: Option<Delta>,
    cool_streak: u32,
    since_heartbeat_secs: f64,
}

impl Watchdog {
    pub(crate) fn new() -> Self {
        Self {
            episode: None,
            cool_streak: 0,
            since_heartbeat_secs: 0.0,
        }
    }

    fn is_hot(d: &Delta) -> bool {
        if d.energy_j > 0.0 {
            d.watts() >= HOT_WATTS
        } else {
            d.p_band_cores() >= HOT_P_BAND_CORES
        }
    }

    pub(crate) fn observe(&mut self, d: &Delta) -> Option<String> {
        let hot = Self::is_hot(d);
        let Some(episode) = self.episode.as_mut() else {
            if !hot {
                return None;
            }
            self.episode = Some(*d);
            self.cool_streak = 0;
            self.since_heartbeat_secs = 0.0;
            return Some(format!(
                "cpu-watch: high CPU began: {} pressure={:?}",
                d.describe(),
                super::sched::pressure(),
            ));
        };

        // Inside an episode every window counts toward its totals, cool or not
        // — the episode is over only after COOL_SAMPLES_TO_END quiet windows.
        episode.accumulate(d);
        if hot {
            self.cool_streak = 0;
            self.since_heartbeat_secs += d.wall_secs;
            let spacing = if episode.wall_secs >= HEARTBEAT_BACKOFF_AFTER_SECS {
                HEARTBEAT_LATE_SECS
            } else {
                HEARTBEAT_EARLY_SECS
            };
            if self.since_heartbeat_secs >= spacing {
                self.since_heartbeat_secs = 0.0;
                return Some(format!(
                    "cpu-watch: still high after {:.0}s: {}",
                    episode.wall_secs,
                    d.describe(),
                ));
            }
            return None;
        }
        self.cool_streak += 1;
        if self.cool_streak < COOL_SAMPLES_TO_END {
            return None;
        }
        let e = self.episode.take().expect("checked above");
        Some(format!(
            "cpu-watch: high CPU ended after {:.0}s: cpu-time {}",
            e.wall_secs,
            e.totals(),
        ))
    }
}

/// How often the resident sampler reads the counters. Two cheap syscalls per
/// tick; the watchdog math uses measured wall time, so timer coalescing on the
/// background thread cannot skew the rates.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Spawn the resident sampler for this process (GUI and daemon call this once
/// at startup). The thread itself runs in the background band so the monitor
/// can never contribute to the problem it watches.
pub(crate) fn start_watchdog() {
    let spawned = std::thread::Builder::new()
        .name("cpu-watch".into())
        .spawn(|| {
            let _ = super::sched::apply(super::sched::Qos::Background);
            let Some(mut prev) = Snapshot::take() else {
                diag::log("cpu-watch: kernel CPU accounting unavailable; monitor off");
                return;
            };
            let mut dog = Watchdog::new();
            loop {
                std::thread::sleep(SAMPLE_INTERVAL);
                let Some(now) = Snapshot::take() else {
                    continue;
                };
                let delta = prev.delta(&now);
                prev = now;
                if let Some(line) = dog.observe(&delta) {
                    diag::log(line);
                }
            }
        });
    if spawned.is_err() {
        diag::log("cpu-watch: sampler thread failed to spawn; monitor off");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window with the given wall seconds, per-class CPU (keyed by the same
    /// labels the log lines use, so a test reads like the line it asserts on),
    /// and energy.
    fn delta(wall: f64, class_secs: &[(&str, f64)], energy_j: f64) -> Delta {
        let mut d = Delta {
            wall_secs: wall,
            qos_secs: [0.0; QOS_CLASSES],
            energy_j,
        };
        for &(label, secs) in class_secs {
            d.qos_secs[qos_slot(label)] = secs;
        }
        d
    }

    /// The 0.5.3 bug signature: ~11W of utility-class CPU. Must open an
    /// episode immediately and close with totals after two cool windows.
    #[test]
    fn watchdog_records_a_p_core_burn_episode() {
        let mut dog = Watchdog::new();
        let hot = delta(5.0, &[("util", 17.5)], 55.0); // 350% utility, 11W
        let start = dog.observe(&hot).expect("onset must log");
        assert!(start.contains("high CPU began"), "{start}");
        assert!(start.contains("util=350%"), "{start}");
        assert!(start.contains("11.0W"), "{start}");

        // One quiet window does not end it…
        let cool = delta(5.0, &[("bg", 0.2)], 1.0);
        assert!(dog.observe(&cool).is_none());
        // …but the second does, and the summary carries the episode totals.
        let end = dog.observe(&cool).expect("second cool window ends it");
        assert!(end.contains("high CPU ended after 15s"), "{end}");
        assert!(end.contains("util=17.5s"), "{end}");
        assert!(end.contains("avg 3.8W"), "{end}");
    }

    /// The healthy new-design walk (~2W, all background band) must never make
    /// noise — E-cluster load cannot spin a fan, however many cores it uses.
    #[test]
    fn watchdog_stays_quiet_for_e_cluster_walk() {
        let mut dog = Watchdog::new();
        let walk = delta(5.0, &[("bg", 14.5), ("util", 0.6)], 10.0); // 290% bg, 2W
        for _ in 0..100 {
            assert!(dog.observe(&walk).is_none());
        }
    }

    /// Intel (no task_energy): the P-band core-count fallback must carry the
    /// same decision, and background-band load must stay exempt there too.
    #[test]
    fn watchdog_core_fallback_when_energy_is_not_accounted() {
        let mut dog = Watchdog::new();
        assert!(dog
            .observe(&delta(5.0, &[("bg", 20.0)], 0.0))
            .is_none(), "background band alone must not trip the fallback");
        assert!(dog
            .observe(&delta(5.0, &[("ui", 15.0)], 0.0))
            .expect("3 user-interactive cores must trip it")
            .contains("ui=300%"));
    }

    /// Heartbeats: every ~30s early, then every ~300s — a permanently hot
    /// process must not be able to flood the size-rotated log.
    #[test]
    fn watchdog_heartbeats_back_off() {
        let mut dog = Watchdog::new();
        let hot = delta(5.0, &[("util", 17.5)], 55.0);
        assert!(dog.observe(&hot).is_some()); // onset
        let mut lines_in_first_10min = 0;
        let mut lines_in_second_10min = 0;
        for i in 1..240 {
            // 5s windows: samples 1..120 are the first 10 minutes.
            if dog.observe(&hot).is_some() {
                if i <= 120 {
                    lines_in_first_10min += 1;
                } else {
                    lines_in_second_10min += 1;
                }
            }
        }
        assert!(
            (5..=8).contains(&lines_in_first_10min),
            "early phase: ~30s spacing then backoff, got {lines_in_first_10min}"
        );
        assert!(
            lines_in_second_10min <= 2,
            "late phase must be ~300s spacing, got {lines_in_second_10min}"
        );
    }

    /// The pass-boundary suffix must be appendable to an existing diag line:
    /// leading separator, total, and at least one per-class part once the pass
    /// burned enough to matter.
    #[test]
    fn pass_summary_formats_an_appendable_suffix() {
        assert_eq!(pass_summary(None), "", "no snapshot → line unchanged");
        let start = Snapshot::take();
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(600) {
            std::hint::black_box(1u64.wrapping_mul(3));
        }
        let s = pass_summary(start);
        assert!(s.starts_with(" | cpu "), "{s}");
        assert!(s.contains("s ("), "total then per-class parts: {s}");
    }

    /// Live end-to-end: burning CPU on a background-QoS thread must show up in
    /// the background slot of a real snapshot delta. Proves the slot indexing
    /// against the real kernel, not just the struct layout in a header.
    ///
    /// The observer must NOT `join()` while the burn runs: a blocked joiner
    /// donates its priority through the turnstile and the burn then bills to
    /// the *joiner's* band (see the module docs) — the flag/sleep wait below
    /// is what the real writer does (channel recv), which donates nothing.
    #[test]
    fn snapshot_accounts_burn_in_the_right_class() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let before = Snapshot::take().expect("rusage v4 readable");
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let done_flag = std::sync::Arc::clone(&done);
        let burn = std::thread::spawn(move || {
            assert_eq!(super::super::sched::apply(super::super::sched::Qos::Background), 0);
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(300) {
                std::hint::black_box(1u64.wrapping_mul(3));
            }
            done_flag.store(true, Ordering::Release);
        });
        while !done.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(10));
        }
        burn.join().unwrap();
        let after = Snapshot::take().unwrap();
        let d = before.delta(&after);
        assert!(
            d.qos_secs[QOS_BACKGROUND] >= 0.15,
            "expected ≥0.15s in the background slot, got {:.3}s (slot indexing broken?)",
            d.qos_secs[QOS_BACKGROUND]
        );
    }
}
