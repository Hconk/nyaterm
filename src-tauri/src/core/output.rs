use std::sync::{Arc, Mutex};

use crate::app_event::{AppEvent, publish_app_event};
use tauri::{AppHandle, Emitter};
use tokio::time::{Duration, sleep};

const OUTPUT_FLUSH_INTERVAL_MS: u64 = 4;
const OUTPUT_FLUSH_THRESHOLD_BYTES: usize = 64 * 1024;

type OutputSink = dyn Fn(String) + Send + Sync + 'static;

#[derive(Default)]
struct OutputState {
    attached: bool,
    pending: String,
    next_flush_id: u64,
    scheduled_flush_id: Option<u64>,
}

/// Shared session-output coalescer used by all terminal backends.
///
/// It batches output before emitting it to the webview to reduce event pressure
/// under high-throughput streams such as `docker compose logs -f`.
pub struct SessionOutputCoalescer {
    sink: Arc<OutputSink>,
    state: Mutex<OutputState>,
}

impl SessionOutputCoalescer {
    pub fn for_app(app: AppHandle, output_event: String) -> Arc<Self> {
        let session_id = output_event
            .strip_prefix("terminal-output-")
            .map(str::to_owned);
        Self::with_sink(move |text| {
            let _ = app.emit(&output_event, &text);
            if let Some(session_id) = &session_id {
                publish_app_event(
                    &app,
                    AppEvent::TerminalOutput {
                        session_id: session_id.clone(),
                        data: text,
                    },
                );
            }
        })
    }

    pub fn with_sink<F>(sink: F) -> Arc<Self>
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        Arc::new(Self {
            sink: Arc::new(sink),
            state: Mutex::new(OutputState::default()),
        })
    }

    pub fn push(self: &Arc<Self>, text: impl AsRef<str>) {
        self.push_owned(text.as_ref().to_string());
    }

    pub fn push_owned(self: &Arc<Self>, text: String) {
        if text.is_empty() {
            return;
        }

        let mut schedule_timer = None;
        let mut flush_now = false;

        {
            let mut state = self.state.lock().unwrap();
            let was_empty = state.pending.is_empty();
            state.pending.push_str(&text);

            if state.attached && state.pending.len() >= OUTPUT_FLUSH_THRESHOLD_BYTES {
                state.next_flush_id = state.next_flush_id.wrapping_add(1);
                state.scheduled_flush_id = None;
                flush_now = true;
            } else if state.attached && was_empty && state.scheduled_flush_id.is_none() {
                state.next_flush_id = state.next_flush_id.wrapping_add(1);
                let flush_id = state.next_flush_id;
                state.scheduled_flush_id = Some(flush_id);
                schedule_timer = Some(flush_id);
            }
        }

        if let Some(flush_id) = schedule_timer {
            self.schedule_flush(flush_id);
        }

        if flush_now {
            self.flush_pending();
        }
    }

    pub fn attach(self: &Arc<Self>) {
        let payload = {
            let mut state = self.state.lock().unwrap();
            state.attached = true;
            state.next_flush_id = state.next_flush_id.wrapping_add(1);
            state.scheduled_flush_id = None;
            take_pending(&mut state)
        };

        if let Some(payload) = payload {
            (self.sink)(payload);
        }
    }

    pub fn close(self: &Arc<Self>) {
        let payload = {
            let mut state = self.state.lock().unwrap();
            state.next_flush_id = state.next_flush_id.wrapping_add(1);
            state.scheduled_flush_id = None;
            take_pending(&mut state)
        };

        if let Some(payload) = payload {
            (self.sink)(payload);
        }
    }

    fn schedule_flush(self: &Arc<Self>, flush_id: u64) {
        let output = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            sleep(Duration::from_millis(OUTPUT_FLUSH_INTERVAL_MS)).await;
            output.flush_if_scheduled(flush_id);
        });
    }

    fn flush_if_scheduled(self: &Arc<Self>, flush_id: u64) {
        let payload = {
            let mut state = self.state.lock().unwrap();
            if state.scheduled_flush_id != Some(flush_id) {
                return;
            }

            state.scheduled_flush_id = None;
            if !state.attached {
                return;
            }

            take_pending(&mut state)
        };

        if let Some(payload) = payload {
            (self.sink)(payload);
        }
    }

    fn flush_pending(self: &Arc<Self>) {
        let payload = {
            let mut state = self.state.lock().unwrap();
            if !state.attached {
                return;
            }

            state.next_flush_id = state.next_flush_id.wrapping_add(1);
            state.scheduled_flush_id = None;
            take_pending(&mut state)
        };

        if let Some(payload) = payload {
            (self.sink)(payload);
        }
    }
}

fn take_pending(state: &mut OutputState) -> Option<String> {
    if state.pending.is_empty() {
        None
    } else {
        Some(std::mem::take(&mut state.pending))
    }
}

#[cfg(test)]
mod tests {
    use super::SessionOutputCoalescer;
    use std::sync::{Arc, Mutex};
    use tokio::time::{Duration, sleep};

    #[tokio::test]
    async fn timer_flush_batches_pending_output() {
        let emitted = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = emitted.clone();
        let output = SessionOutputCoalescer::with_sink(move |text| {
            sink.lock().unwrap().push(text);
        });

        output.attach();
        output.push("hello");
        output.push(" world");

        sleep(Duration::from_millis(20)).await;

        assert_eq!(emitted.lock().unwrap().as_slice(), ["hello world"]);
    }

    #[tokio::test]
    async fn size_threshold_flushes_immediately() {
        let emitted = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = emitted.clone();
        let output = SessionOutputCoalescer::with_sink(move |text| {
            sink.lock().unwrap().push(text);
        });

        output.attach();
        output.push_owned("x".repeat(64 * 1024));

        assert_eq!(emitted.lock().unwrap().len(), 1);
        assert_eq!(emitted.lock().unwrap()[0].len(), 64 * 1024);
    }

    #[tokio::test]
    async fn attach_flushes_pre_attach_output() {
        let emitted = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = emitted.clone();
        let output = SessionOutputCoalescer::with_sink(move |text| {
            sink.lock().unwrap().push(text);
        });

        output.push("before attach");
        sleep(Duration::from_millis(20)).await;
        assert!(emitted.lock().unwrap().is_empty());

        output.attach();

        assert_eq!(emitted.lock().unwrap().as_slice(), ["before attach"]);
    }

    #[tokio::test]
    async fn close_flushes_remaining_output() {
        let emitted = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = emitted.clone();
        let output = SessionOutputCoalescer::with_sink(move |text| {
            sink.lock().unwrap().push(text);
        });

        output.attach();
        output.push("pending");
        output.close();

        assert_eq!(emitted.lock().unwrap().as_slice(), ["pending"]);
    }
}
