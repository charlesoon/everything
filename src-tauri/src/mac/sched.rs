//! Background-scheduling hints for the macOS indexer.
//!
//! Nothing in the indexing pipeline used to set a QoS class, so every scan
//! thread inherited the Tauri main thread's `USER_INTERACTIVE` class (macOS
//! propagates the creating thread's QoS through `pthread_create`). On Apple
//! Silicon that pins the entire fan-out onto performance cores at full clock,
//! which is why a fresh index of a ~1M-entry home held 400%+ CPU and spun up
//! the fans on an M2 Pro.
//!
//! [`apply`] fixes that with one per-thread call, from inside the thread it
//! affects. `QOS_CLASS_UTILITY` makes the scheduler prefer efficiency cores and
//! only spill onto performance cores when those are idle; `QOS_CLASS_BACKGROUND`
//! is stricter (E-cores only) at roughly 3x the wall time, so it is reserved for
//! restart catchup and for passes running under thermal or battery pressure.
//!
//! ## Why there is no `setiopolicy_np` call here
//!
//! Pairing the QoS class with an explicit `setiopolicy_np(IOPOL_TYPE_DISK,
//! IOPOL_SCOPE_THREAD, IOPOL_UTILITY)` looks like the obvious way to also
//! deprioritize the scan's reads. It does the opposite: the legacy per-thread
//! I/O policy conflicts with the QoS system, so setting it **unsets the QoS
//! class** (`pthread_get_qos_class_np` then reports `QOS_CLASS_UNSPECIFIED`) and
//! permanently opts the thread out — every later `pthread_set_qos_class_self_np`
//! returns `EPERM`. Both calls still report success, so the damage is silent;
//! `apply_sets_the_class_and_stays_qos_managed` below is what pins this down.
//!
//! A QoS class already carries its own I/O tier — `BACKGROUND` implies throttled
//! disk I/O — so the class alone is both necessary and sufficient. The thread's
//! explicit I/O policy must stay `IOPOL_DEFAULT`, which is the kernel's marker
//! for "derive this from QoS".
//!
//! [`pressure`] additionally reads `NSProcessInfo` so a pass can shrink its own
//! fan-out when the machine is already hot or in Low Power Mode.

use std::ffi::c_void;
use std::os::raw::c_char;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

// <sys/qos.h> — qos_class_t
const QOS_CLASS_UTILITY: u32 = 0x11;
const QOS_CLASS_BACKGROUND: u32 = 0x09;

extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    fn pthread_self() -> *mut c_void;
    fn pthread_get_qos_class_np(
        thread: *mut c_void,
        qos_class: *mut u32,
        relative_priority: *mut i32,
    ) -> i32;
}

/// How far to step the calling thread out of the foreground's way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qos {
    /// E-core preferred, may still use P-cores when they are idle. The default
    /// for a fresh index: the user is waiting for it, so it should not be
    /// gratuitously slow — just cheap enough not to spin the fans.
    Utility,
    /// E-core only plus a throttled I/O tier. Used for restart catchup (nobody
    /// is waiting on it) and whenever [`pressure`] reports the machine is hot or
    /// in Low Power Mode.
    Background,
}

/// Apply `qos` to the **calling** thread. Must run on the thread it affects —
/// there is no macOS API to retune another running thread's QoS.
///
/// Returns the errno (0 on success). Failure is survivable — the thread keeps
/// its inherited scheduling, i.e. the old behavior — but it is the difference
/// between quiet fans and not, so callers should not discard it silently.
pub fn apply(qos: Qos) -> i32 {
    let qos_class = match qos {
        Qos::Utility => QOS_CLASS_UTILITY,
        Qos::Background => QOS_CLASS_BACKGROUND,
    };
    unsafe { pthread_set_qos_class_self_np(qos_class, 0) }
}

/// The calling thread's effective QoS class as the kernel reports it, for
/// logging. `"unspecified"` means the thread is opted out of the QoS system —
/// the failure mode described at the top of this file, and the one worth
/// noticing in a field log because it brings the CPU spike back.
pub fn current_class_name() -> &'static str {
    let mut qos_class = 0u32;
    let mut relative_priority = 0i32;
    if unsafe { pthread_get_qos_class_np(pthread_self(), &mut qos_class, &mut relative_priority) }
        != 0
    {
        return "unreadable";
    }
    match qos_class {
        0x21 => "user-interactive",
        0x19 => "user-initiated",
        0x15 => "default",
        QOS_CLASS_UTILITY => "utility",
        QOS_CLASS_BACKGROUND => "background",
        _ => "unspecified",
    }
}

/// System-level reasons to index more gently, worst-first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pressure {
    /// Cool and not power-saving — index at the normal background pace.
    None,
    /// Thermal state `fair`: warm but not yet throttling. Halve the fan-out.
    Elevated,
    /// Thermal state `serious`/`critical`, or Low Power Mode is on. Use the
    /// smallest viable fan-out and duty-cycle the walk.
    High,
}

// -- NSProcessInfo bridge ---------------------------------------------------
//
// Reaching NSProcessInfo through objc_msgSend directly keeps this dependency
// free (libobjc is already linked by Tauri/AppKit) and avoids pulling in an objc
// crate for three selectors. arm64 has no variadic objc_msgSend, so every call
// site casts the symbol to its exact signature.

type MsgSendId = unsafe extern "C" fn(*const c_void, *const c_void) -> *const c_void;
type MsgSendI64 = unsafe extern "C" fn(*const c_void, *const c_void) -> i64;
/// `BOOL` is `signed char` on x86_64 and `bool` on arm64; reading the low byte
/// as `i8` is correct for both.
type MsgSendI8 = unsafe extern "C" fn(*const c_void, *const c_void) -> i8;
type MsgSendSelI8 = unsafe extern "C" fn(*const c_void, *const c_void, *const c_void) -> i8;

extern "C" {
    fn objc_getClass(name: *const c_char) -> *const c_void;
    fn sel_registerName(name: *const c_char) -> *const c_void;
    fn objc_msgSend();
}

fn sel(name: &[u8]) -> *const c_void {
    // Every literal passed here is NUL-terminated at the call site.
    unsafe { sel_registerName(name.as_ptr() as *const c_char) }
}

/// `[NSProcessInfo processInfo]`, or null when the class is unavailable.
fn process_info() -> *const c_void {
    let class = unsafe { objc_getClass(b"NSProcessInfo\0".as_ptr() as *const c_char) };
    if class.is_null() {
        return std::ptr::null();
    }
    let msg_send: MsgSendId = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { msg_send(class, sel(b"processInfo\0")) }
}

fn responds_to(obj: *const c_void, selector: *const c_void) -> bool {
    let msg_send: MsgSendSelI8 = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { msg_send(obj, sel(b"respondsToSelector:\0"), selector) != 0 }
}

/// Uncached probe. `NSProcessInfoThermalState`: nominal 0, fair 1, serious 2,
/// critical 3. `isLowPowerModeEnabled` is macOS 12+, hence the responds-to
/// guard (Tauri v2 still supports 10.15).
fn probe_pressure() -> Pressure {
    let info = process_info();
    if info.is_null() {
        return Pressure::None;
    }

    let low_power_sel = sel(b"isLowPowerModeEnabled\0");
    if responds_to(info, low_power_sel) {
        let msg_send: MsgSendI8 = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
        if unsafe { msg_send(info, low_power_sel) } != 0 {
            return Pressure::High;
        }
    }

    let thermal_sel = sel(b"thermalState\0");
    if responds_to(info, thermal_sel) {
        let msg_send: MsgSendI64 = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
        return match unsafe { msg_send(info, thermal_sel) } {
            0 => Pressure::None,
            1 => Pressure::Elevated,
            _ => Pressure::High,
        };
    }

    Pressure::None
}

/// Cached for [`PRESSURE_TTL`] so the scan loop can poll per batch without
/// paying for an Objective-C round trip on every entry.
const PRESSURE_TTL: Duration = Duration::from_secs(2);
static PRESSURE_CACHE: Mutex<Option<(Instant, Pressure)>> = Mutex::new(None);

/// Current thermal / power pressure, refreshed at most every 2s.
pub fn pressure() -> Pressure {
    let mut cache = PRESSURE_CACHE.lock();
    if let Some((at, value)) = *cache {
        if at.elapsed() < PRESSURE_TTL {
            return value;
        }
    }
    // Probing while holding the lock collapses concurrent callers onto one round
    // trip instead of letting every scan worker issue its own.
    let value = probe_pressure();
    *cache = Some((Instant::now(), value));
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    // <sys/resource.h>
    const IOPOL_TYPE_DISK: i32 = 0;
    const IOPOL_SCOPE_THREAD: i32 = 1;
    /// "Derived from QoS" — the state a QoS-managed thread must stay in.
    const IOPOL_DEFAULT: i32 = 0;

    extern "C" {
        fn getiopolicy_np(iotype: i32, scope: i32) -> i32;
    }

    /// What the kernel actually recorded for the calling thread: (QoS class,
    /// explicit disk I/O policy).
    fn current_settings() -> (u32, i32) {
        let mut qos_class = 0u32;
        let mut relative_priority = 0i32;
        unsafe {
            assert_eq!(
                pthread_get_qos_class_np(pthread_self(), &mut qos_class, &mut relative_priority),
                0,
                "pthread_get_qos_class_np failed"
            );
            (qos_class, getiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD))
        }
    }

    #[test]
    fn apply_sets_the_class_and_stays_qos_managed() {
        // The entire fix rests on this syscall landing, so assert on the
        // kernel's own readback rather than on "it did not panic" — that is what
        // caught the setiopolicy_np conflict documented at the top of this file.
        //
        // The IOPOL_DEFAULT half is a regression guard: if anyone reintroduces a
        // per-thread setiopolicy_np call, the class silently reverts to
        // QOS_CLASS_UNSPECIFIED and the fans come back.
        std::thread::spawn(|| {
            assert_eq!(apply(Qos::Utility), 0, "setting UTILITY reported an errno");
            assert_eq!(current_settings(), (QOS_CLASS_UTILITY, IOPOL_DEFAULT));

            // Re-applying on a live thread must keep working: a worker can be
            // retuned, and an opted-out thread would fail with EPERM here.
            assert_eq!(apply(Qos::Background), 0, "setting BACKGROUND reported an errno");
            assert_eq!(current_settings(), (QOS_CLASS_BACKGROUND, IOPOL_DEFAULT));
        })
        .join()
        .expect("scheduling hints must not panic");
    }

    #[test]
    fn apply_leaves_other_threads_alone() {
        // QoS is strictly per-thread: a scan worker lowering itself must not
        // drag the UI thread down with it.
        let before = current_settings();
        std::thread::spawn(|| apply(Qos::Background)).join().unwrap();
        assert_eq!(current_settings(), before);
    }

    #[test]
    fn pressure_probe_returns_a_value_and_caches_it() {
        // Exercises the whole objc_msgSend bridge: a wrong signature or a bad
        // selector name would trap here rather than in a release build.
        let first = pressure();
        assert_eq!(first, pressure(), "second call must hit the cache");
    }
}
