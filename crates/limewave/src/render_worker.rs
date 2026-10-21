//! Renders that take too long for a frame (a generated tail, a spectrum loop) run on a worker
//! thread, so the window stays responsive (v0.3 §6). A dialog restarts its render after a
//! pause in the settings ([`RenderSlot`]), and a result made for an edit that is no longer
//! current is thrown away.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui;

/// What a render was made for: the file, its edit (the history position and the length of its
/// effect list) and a hash of the dialog's settings. A result is used only while the key it
/// was rendered for is still the current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RenderKey {
    /// The index of the file in the file list.
    pub file: usize,
    pub effects_len: usize,
    pub history_cursor: usize,
    pub settings_hash: u64,
}

/// A render running on a worker thread.
pub struct RenderJob<T> {
    pub key: RenderKey,
    rx: Receiver<T>,
}

impl<T> RenderJob<T> {
    /// The result, once the render has finished; `None` while it runs. A result is handed out
    /// once.
    pub fn poll(&self) -> Option<T> {
        self.rx.try_recv().ok()
    }
}

/// Asks its context for a repaint when dropped, however the worker ends.
struct RepaintOnDrop(egui::Context);

impl Drop for RepaintOnDrop {
    fn drop(&mut self) {
        self.0.request_repaint();
    }
}

/// Runs `f` on a new thread for `key` and asks `ctx` for a repaint when it is done, so the
/// next frame can pick the result up, or see that the worker died without one (a panic).
/// Dropping the job lets the render finish unseen.
pub fn spawn_render<T: Send + 'static>(
    key: RenderKey,
    ctx: egui::Context,
    f: impl FnOnce() -> T + Send + 'static,
) -> RenderJob<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Locals drop in reverse order, also when `f` panics: the sender goes first, so the
        // repaint finds the result sent, or the channel closed.
        let _repaint = RepaintOnDrop(ctx);
        let tx = tx;
        let _ = tx.send(f());
    });
    RenderJob { key, rx }
}

/// One dialog's render: the job running, the last result kept (or the key whose render
/// failed) and when the settings last changed, for the pause before the next render.
pub struct RenderSlot<T> {
    job: Option<RenderJob<T>>,
    result: Option<(RenderKey, T)>,
    /// The key whose worker died without a result; it is not started again until the key
    /// changes.
    failed: Option<RenderKey>,
    changed_at: Option<Instant>,
}

impl<T> Default for RenderSlot<T> {
    fn default() -> Self {
        RenderSlot {
            job: None,
            result: None,
            failed: None,
            changed_at: None,
        }
    }
}

impl<T> RenderSlot<T> {
    /// The settings changed: the running render and the kept result are dropped, and the
    /// next render waits for a pause.
    pub fn restart(&mut self) {
        self.job = None;
        self.result = None;
        self.failed = None;
        self.changed_at = Some(Instant::now());
    }

    /// Whether a render for `key` should start now: nothing for `key` is running, kept or
    /// failed, and `debounce` has passed since the last [`RenderSlot::restart`]. While that
    /// pause runs, a repaint is asked for when it ends.
    pub fn wants(&self, key: RenderKey, debounce: Duration, ctx: &egui::Context) -> bool {
        if self.job.as_ref().is_some_and(|j| j.key == key)
            || self.result.as_ref().is_some_and(|(k, _)| *k == key)
            || self.failed == Some(key)
        {
            return false;
        }
        let waited = self.changed_at.map_or(debounce, |t| t.elapsed());
        if waited < debounce {
            ctx.request_repaint_after(debounce - waited);
            return false;
        }
        true
    }

    /// Makes `job` the running render, dropping any other.
    pub fn start(&mut self, job: RenderJob<T>) {
        self.job = Some(job);
    }

    /// The key of the running render.
    pub fn job_key(&self) -> Option<RenderKey> {
        self.job.as_ref().map(|j| j.key)
    }

    pub fn is_rendering(&self) -> bool {
        self.job.is_some()
    }

    /// Takes the running render's result once it has finished, or waits for it when `block`.
    /// The result is kept only when its key is `current`; otherwise it is discarded. A worker
    /// that died without a result ends the job too, and, when its key is `current`, marks that
    /// key as failed ([`RenderSlot::failed`]).
    pub fn finish(&mut self, current: Option<RenderKey>, block: bool) {
        let Some(job) = &self.job else { return };
        let got = if block {
            job.rx.recv().ok()
        } else {
            match job.rx.try_recv() {
                Ok(v) => Some(v),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => None,
            }
        };
        let key = job.key;
        self.job = None;
        if current != Some(key) {
            return;
        }
        match got {
            Some(v) => self.result = Some((key, v)),
            None => self.failed = Some(key),
        }
    }

    /// Whether the render for `current` failed: its worker died without a result.
    pub fn failed(&self, current: Option<RenderKey>) -> bool {
        current.is_some() && self.failed == current
    }

    /// The kept result, if it was rendered for `current`.
    pub fn result(&self, current: Option<RenderKey>) -> Option<&T> {
        match &self.result {
            Some((k, v)) if Some(*k) == current => Some(v),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key(settings_hash: u64) -> RenderKey {
        RenderKey {
            file: 0,
            effects_len: 0,
            history_cursor: 0,
            settings_hash,
        }
    }

    /// A context that counts its repaint requests.
    fn counting_ctx() -> (egui::Context, Arc<AtomicUsize>) {
        let ctx = egui::Context::default();
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        ctx.set_repaint_observer(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        (ctx, count)
    }

    #[test]
    fn render_job_delivers_and_repaints() {
        let (ctx, repaints) = counting_ctx();
        let (go, wait) = mpsc::channel::<()>();
        let job = spawn_render(key(1), ctx, move || {
            wait.recv().unwrap();
            42
        });
        assert_eq!(job.key, key(1));
        assert_eq!(job.poll(), None, "still running");
        assert_eq!(repaints.load(Ordering::SeqCst), 0);
        go.send(()).unwrap();
        let start = Instant::now();
        let got = loop {
            if let Some(v) = job.poll() {
                break v;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "never finished");
            std::thread::yield_now();
        };
        assert_eq!(got, 42);
        // The repaint follows the send; give the worker a moment to make it.
        while repaints.load(Ordering::SeqCst) == 0 {
            assert!(start.elapsed() < Duration::from_secs(10), "no repaint");
            std::thread::yield_now();
        }
        assert_eq!(job.poll(), None, "handed out once");
    }

    #[test]
    fn slot_keeps_a_current_result_and_discards_a_stale_one() {
        let ctx = egui::Context::default();
        let mut slot = RenderSlot::default();
        slot.start(spawn_render(key(1), ctx.clone(), || 1));
        assert_eq!(slot.job_key(), Some(key(1)));
        slot.finish(Some(key(1)), true);
        assert!(!slot.is_rendering());
        assert_eq!(slot.result(Some(key(1))), Some(&1));
        assert_eq!(slot.result(Some(key(2))), None, "not for another key");

        slot.start(spawn_render(key(2), ctx, || 2));
        slot.finish(Some(key(3)), true);
        assert!(!slot.is_rendering(), "the stale job is gone");
        assert_eq!(slot.result(Some(key(2))), None, "and its result discarded");
        assert_eq!(slot.result(Some(key(1))), Some(&1), "the old result stays");
    }

    #[test]
    fn slot_waits_for_a_pause_after_a_change() {
        let ctx = egui::Context::default();
        let mut slot: RenderSlot<i32> = RenderSlot::default();
        let long = Duration::from_secs(3600);
        assert!(slot.wants(key(1), long, &ctx), "nothing changed yet");
        slot.restart();
        assert!(!slot.wants(key(1), long, &ctx), "inside the pause");
        assert!(slot.wants(key(1), Duration::ZERO, &ctx), "no pause");

        slot.start(spawn_render(key(1), ctx.clone(), || 1));
        assert!(!slot.wants(key(1), Duration::ZERO, &ctx), "running");
        assert!(slot.wants(key(2), Duration::ZERO, &ctx), "another key");
        slot.finish(Some(key(1)), true);
        assert!(!slot.wants(key(1), Duration::ZERO, &ctx), "kept");
        slot.restart();
        assert_eq!(slot.result(Some(key(1))), None, "a change drops the result");
        assert!(slot.wants(key(1), Duration::ZERO, &ctx));
    }

    #[test]
    fn a_worker_that_dies_ends_the_job() {
        let ctx = egui::Context::default();
        let mut slot: RenderSlot<i32> = RenderSlot::default();
        slot.start(spawn_render(key(1), ctx, || panic!("render failed")));
        slot.finish(Some(key(1)), true);
        assert!(!slot.is_rendering());
        assert_eq!(slot.result(Some(key(1))), None);
    }

    #[test]
    fn a_failed_render_is_not_started_again_for_its_key() {
        let (ctx, repaints) = counting_ctx();
        let mut slot: RenderSlot<i32> = RenderSlot::default();
        slot.start(spawn_render(key(1), ctx.clone(), || {
            panic!("render failed")
        }));
        // The worker asks for a repaint even though it died, so the failure is seen.
        let start = Instant::now();
        while repaints.load(Ordering::SeqCst) == 0 {
            assert!(start.elapsed() < Duration::from_secs(10), "no repaint");
            std::thread::yield_now();
        }
        slot.finish(Some(key(1)), true);
        assert!(slot.failed(Some(key(1))), "recorded as failed");
        assert!(!slot.wants(key(1), Duration::ZERO, &ctx), "not respawned");
        assert!(!slot.failed(Some(key(2))), "only for its key");
        assert!(
            slot.wants(key(2), Duration::ZERO, &ctx),
            "another key renders"
        );
        slot.restart();
        assert!(!slot.failed(Some(key(1))), "a change clears it");
        assert!(slot.wants(key(1), Duration::ZERO, &ctx));

        // A stale failure is not recorded.
        slot.start(spawn_render(key(3), ctx, || panic!("render failed")));
        slot.finish(Some(key(4)), true);
        assert!(!slot.failed(Some(key(3))) && !slot.failed(Some(key(4))));
    }
}
