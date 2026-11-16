//! Native UI task executor and coalescing cross-thread event channel.
//!
//! The indexer only knows this sink. It does not own a window or a renderer.
//! At most one wake-up and one payload per event kind can wait for the UI.

use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone)]
pub(crate) struct EventSink {
    latest: Arc<Mutex<BTreeMap<&'static str, Value>>>,
    wake: async_channel::Sender<()>,
}

impl EventSink {
    pub(crate) fn channel() -> (Self, async_channel::Receiver<()>) {
        let (wake, receiver) = async_channel::bounded(1);
        (
            Self {
                latest: Arc::default(),
                wake,
            },
            receiver,
        )
    }

    pub(crate) fn emit(&self, name: &'static str, payload: impl Serialize) -> Result<(), String> {
        let value = serde_json::to_value(payload).map_err(|error| error.to_string())?;
        self.latest.lock().insert(name, value);
        let _ = self.wake.try_send(());
        Ok(())
    }

    pub(crate) fn drain(&self) -> BTreeMap<&'static str, Value> {
        std::mem::take(&mut *self.latest.lock())
    }

    pub(crate) fn exit(&self, code: i32) {
        std::process::exit(code);
    }

    pub(crate) fn restart(&self) {
        if let Ok(exe) = std::env::current_exe() {
            if std::process::Command::new(exe).spawn().is_ok() {
                std::process::exit(0);
            }
        }
    }
}

fn executor() -> &'static tokio::runtime::Runtime {
    static EXECUTOR: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    EXECUTOR.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(8)
            .thread_name("everything-worker")
            .enable_time()
            .build()
            .expect("create background executor")
    })
}

pub(crate) fn spawn_blocking<F, R>(work: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    executor().spawn_blocking(work)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_ui_receives_only_latest_payload_per_kind() {
        let (sink, wake) = EventSink::channel();
        for count in 0..10_000 {
            sink.emit("index_updated", count).unwrap();
        }
        sink.emit("service_health", "ready").unwrap();
        assert_eq!(wake.len(), 1);
        let payloads = sink.drain();
        assert_eq!(payloads["index_updated"], 9999);
        assert_eq!(payloads["service_health"], "ready");
        assert!(sink.drain().is_empty());
    }
}
