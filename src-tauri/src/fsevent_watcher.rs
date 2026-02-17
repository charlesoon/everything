use fsevent_sys as fs;
use fsevent_sys::core_foundation as cf;
use fsevent_sys::core_foundation::CFRunLoopRef;

use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

const LATENCY_SECONDS: cf::CFTimeInterval = 0.3;

pub enum FsEvent {
    Paths(Vec<PathBuf>),
    MustScanSubDirs(PathBuf),
    HistoryDone,
}

pub struct FsEventWatcher {
    last_event_id: Arc<AtomicU64>,
    run_loop_ref: Option<cf::CFRunLoopRef>,
    thread_handle: Option<thread::JoinHandle<()>>,
}

unsafe impl Send for FsEventWatcher {}
unsafe impl Sync for FsEventWatcher {}

struct CallbackInfo {
    tx: mpsc::Sender<FsEvent>,
    last_event_id: Arc<AtomicU64>,
}

extern "C" {
    fn CFRunLoopIsWaiting(runloop: CFRunLoopRef) -> cf::Boolean;
}

extern "C" fn stream_callback(
    _stream_ref: fs::FSEventStreamRef,
    info: *mut std::os::raw::c_void,
    num_events: usize,
    event_paths: *mut std::os::raw::c_void,
    event_flags: *const fs::FSEventStreamEventFlags,
    event_ids: *const fs::FSEventStreamEventId,
) {
    unsafe {
        let info = &*(info as *const CallbackInfo);
        let paths_ptr = event_paths as *const *const std::os::raw::c_char;
        let mut normal_paths: Vec<PathBuf> = Vec::new();

        for i in 0..num_events {
            let flag = *event_flags.add(i);
            let event_id = *event_ids.add(i);

            let mut prev = info.last_event_id.load(Ordering::Relaxed);
            while event_id > prev {
                match info.last_event_id.compare_exchange_weak(
                    prev,
                    event_id,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => prev = actual,
                }
            }

            let c_str = CStr::from_ptr(*paths_ptr.add(i));
            let path = PathBuf::from(c_str.to_string_lossy().into_owned());

            if flag & fs::kFSEventStreamEventFlagHistoryDone != 0 {
                let _ = info.tx.send(FsEvent::HistoryDone);
                continue;
            }

            if flag & fs::kFSEventStreamEventFlagMustScanSubDirs != 0 {
                let _ = info.tx.send(FsEvent::MustScanSubDirs(path));
                continue;
            }

            normal_paths.push(path);
        }

        if !normal_paths.is_empty() {
            let _ = info.tx.send(FsEvent::Paths(normal_paths));
        }
    }
}

impl FsEventWatcher {
    pub fn new(
        root: &Path,
        since_event_id: Option<u64>,
        tx: mpsc::Sender<FsEvent>,
    ) -> Result<Self, String> {
        let since_when = since_event_id.unwrap_or(fs::kFSEventStreamEventIdSinceNow);
        let last_event_id = Arc::new(AtomicU64::new(since_when));

        let root_str = root
            .to_str()
            .ok_or_else(|| "root path is not valid UTF-8".to_string())?;

        let context_info = Box::new(CallbackInfo {
            tx,
            last_event_id: Arc::clone(&last_event_id),
        });
        let context_ptr = Box::into_raw(context_info);

        let stream_context = fs::FSEventStreamContext {
            version: 0,
            info: context_ptr as *mut std::os::raw::c_void,
            retain: None,
            release: None,
            copy_description: None,
        };

        let flags = fs::kFSEventStreamCreateFlagFileEvents | fs::kFSEventStreamCreateFlagNoDefer;

        let stream = unsafe {
            let c_path = std::ffi::CString::new(root_str).map_err(|e| e.to_string())?;
            let cf_string = cf::CFStringCreateWithCString(
                cf::kCFAllocatorDefault,
                c_path.as_ptr(),
                cf::kCFStringEncodingUTF8,
            );
            if cf_string.is_null() {
                drop(Box::from_raw(context_ptr));
                return Err("failed to create CFString for root path".to_string());
            }

            let cf_array =
                cf::CFArrayCreateMutable(cf::kCFAllocatorDefault, 0, &cf::kCFTypeArrayCallBacks);
            cf::CFArrayAppendValue(cf_array, cf_string);
            cf::CFRelease(cf_string);

            let s = fs::FSEventStreamCreate(
                cf::kCFAllocatorDefault,
                stream_callback,
                &stream_context,
                cf_array,
                since_when,
                LATENCY_SECONDS,
                flags,
            );
            cf::CFRelease(cf_array);

            if s.is_null() {
                drop(Box::from_raw(context_ptr));
                return Err("FSEventStreamCreate returned null".to_string());
            }
            s
        };

        let stream_addr = stream as usize;
        let context_addr = context_ptr as usize;

        let (rl_tx, rl_rx) = std::sync::mpsc::channel::<usize>();

        let thread_handle = thread::Builder::new()
            .name("everything-fsevents".to_string())
            .spawn(move || unsafe {
                let stream = stream_addr as *mut std::os::raw::c_void;
                let context_ptr = context_addr as *mut CallbackInfo;
                let cur_runloop = cf::CFRunLoopGetCurrent();
                fs::FSEventStreamScheduleWithRunLoop(
                    stream,
                    cur_runloop,
                    cf::kCFRunLoopDefaultMode,
                );
                fs::FSEventStreamStart(stream);
                let _ = rl_tx.send(cur_runloop as usize);

                cf::CFRunLoopRun();

                fs::FSEventStreamStop(stream);
                fs::FSEventStreamInvalidate(stream);
                fs::FSEventStreamRelease(stream);
                drop(Box::from_raw(context_ptr));
            })
            .map_err(|e| format!("failed to spawn FSEvents thread: {}", e))?;

        let run_loop_ref = rl_rx
            .recv()
            .map_err(|_| "FSEvents thread terminated before sending run loop ref".to_string())?
            as *mut std::os::raw::c_void;

        Ok(Self {
            last_event_id,
            run_loop_ref: Some(run_loop_ref),
            thread_handle: Some(thread_handle),
        })
    }

    pub fn last_event_id(&self) -> u64 {
        self.last_event_id.load(Ordering::Acquire)
    }

    pub fn stop(&mut self) {
        if let Some(rl) = self.run_loop_ref.take() {
            unsafe {
                while CFRunLoopIsWaiting(rl) == 0 {
                    thread::yield_now();
                }
                cf::CFRunLoopStop(rl);
            }
            if let Some(handle) = self.thread_handle.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for FsEventWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}
