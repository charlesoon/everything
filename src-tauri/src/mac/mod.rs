pub mod bridge;
pub mod cpu_monitor;
pub mod fsevent_watcher;
pub mod launch_agent;
pub mod prefs;

/// Full Disk Access probe that proves an actual protected READ (design D6):
/// `fs::metadata` on TCC.db can succeed without FDA, but reading a byte of it
/// cannot. Shared by the daemon's supervision tick and the GUI's
/// `check_full_disk_access`. Side effect worth keeping: a real read attempt is
/// also what makes macOS list the app (toggled off) in the FDA settings pane.
pub(crate) fn probe_full_disk_access() -> bool {
    use std::io::Read;
    std::fs::File::open("/Library/Application Support/com.apple.TCC/TCC.db")
        .and_then(|mut f| {
            let mut byte = [0u8; 1];
            f.read_exact(&mut byte)
        })
        .is_ok()
}
pub mod sched;
pub mod spotlight_search;
