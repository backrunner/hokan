use std::{
    cell::Cell,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use crate::{
    completion::{CompletionContext, CompletionEngine, ProviderOutput},
    diagnostics::DebugLog,
};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};

pub(super) struct ProviderResult {
    pub(super) context: Arc<CompletionContext>,
    pub(super) output: ProviderOutput,
    pub(super) final_batch: bool,
}

pub(super) struct ProviderWorker {
    sender: Option<Sender<Arc<CompletionContext>>>,
    pending: Receiver<Arc<CompletionContext>>,
    results: Receiver<ProviderResult>,
    latest_query: Arc<AtomicU64>,
    engine: Arc<RwLock<Arc<CompletionEngine>>>,
    join: Option<JoinHandle<()>>,
}

impl ProviderWorker {
    pub(super) fn start(
        engine: Arc<CompletionEngine>,
        debug_log: Option<DebugLog>,
    ) -> crate::Result<Self> {
        let (sender, receiver) = bounded::<Arc<CompletionContext>>(1);
        let pending = receiver.clone();
        let (result_sender, results) = unbounded();
        let latest_query = Arc::new(AtomicU64::new(0));
        let worker_latest_query = Arc::clone(&latest_query);
        let engine = Arc::new(RwLock::new(engine));
        let worker_engine = Arc::clone(&engine);
        let join = thread::Builder::new()
            .name("hokan-providers".into())
            .spawn(move || {
                while let Ok(context) = receiver.recv() {
                    let query_id = context.query_id.get();
                    // A dequeued query that is already superseded would burn
                    // provider budget on batches the runtime discards as
                    // stale, delaying the fresh query queued behind it.
                    // `schedule` publishes the newest id BEFORE enqueueing,
                    // so the fresher query is in the channel already or
                    // arrives right after the skip.
                    if worker_latest_query.load(Ordering::Acquire) != query_id {
                        continue;
                    }
                    let engine = match worker_engine.read() {
                        Ok(engine) => Arc::clone(&engine),
                        Err(_) => break,
                    };
                    let disconnected = Cell::new(false);
                    engine.complete_incremental_with_metrics(
                        &context,
                        |output, final_batch| {
                            if result_sender
                                .send(ProviderResult {
                                    context: Arc::clone(&context),
                                    output,
                                    final_batch,
                                })
                                .is_err()
                            {
                                disconnected.set(true);
                            }
                        },
                        || {
                            disconnected.get()
                                || worker_latest_query.load(Ordering::Acquire) != query_id
                        },
                        |metric| {
                            if let Some(log) = &debug_log {
                                log.provider_finished(
                                    metric.provider,
                                    metric.duration,
                                    metric.candidate_count,
                                    metric.cancelled,
                                );
                            }
                        },
                    );
                    if disconnected.get() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            pending,
            results,
            latest_query,
            engine,
            join: Some(join),
        })
    }

    pub(super) fn schedule(&self, context: Arc<CompletionContext>) -> crate::Result<()> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(crate::Error::Runtime("provider worker is closed".into()));
        };
        self.latest_query
            .store(context.query_id.get(), Ordering::Release);
        match sender.try_send(context) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(context)) => {
                let _ = self.pending.try_recv();
                sender
                    .try_send(context)
                    .map_err(|_| crate::Error::Runtime("provider worker is unavailable".into()))
            }
            Err(TrySendError::Disconnected(_)) => {
                Err(crate::Error::Runtime("provider worker is closed".into()))
            }
        }
    }

    pub(super) const fn results(&self) -> &Receiver<ProviderResult> {
        &self.results
    }

    pub(super) fn replace_engine(&self, engine: Arc<CompletionEngine>) -> crate::Result<()> {
        *self
            .engine
            .write()
            .map_err(|_| crate::Error::Runtime("provider engine was poisoned".into()))? = engine;
        Ok(())
    }
}

impl Drop for ProviderWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CandidateProvider, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };
    use crossbeam_channel::RecvTimeoutError;

    /// Every `complete` call announces its query id and then blocks until the
    /// test releases it, so scheduling/interleaving is fully deterministic.
    struct BlockingProvider {
        started: Sender<u64>,
        release: Receiver<()>,
    }

    impl CandidateProvider for BlockingProvider {
        fn id(&self) -> &'static str {
            "blocking"
        }

        fn applies(&self, _: &CompletionContext) -> bool {
            true
        }

        fn complete(&self, context: &CompletionContext) -> ProviderOutput {
            let _ = self.started.send(context.query_id.get());
            let _ = self.release.recv();
            ProviderOutput::default()
        }
    }

    fn context(query_id: u64) -> Arc<CompletionContext> {
        Arc::new(
            CompletionContext::new(
                QueryId::new(query_id),
                ShellKind::Zsh,
                PathBuf::from("/tmp"),
                BufferSnapshot::new("x", 1, BufferRevision::new(1), SyncQuality::Exact)
                    .expect("buffer"),
            )
            .expect("context"),
        )
    }

    #[test]
    fn burst_coalesces_to_the_newest_query_without_running_superseded_ones() {
        let (started_sender, started) = unbounded();
        let (release, released) = unbounded::<()>();
        let mut engine = CompletionEngine::new(8, 12);
        engine.register(BlockingProvider {
            started: started_sender,
            release: released,
        });
        let worker = ProviderWorker::start(Arc::new(engine), None).expect("worker");

        // The first query starts running and blocks inside the provider.
        worker.schedule(context(1)).expect("schedule q1");
        assert_eq!(
            started.recv_timeout(Duration::from_secs(5)),
            Ok(1),
            "q1 must reach the provider"
        );

        // A burst of newer queries lands while q1 is stuck: q2 is replaced by
        // q3 in the single pending slot before the worker ever sees it.
        worker.schedule(context(2)).expect("schedule q2");
        worker.schedule(context(3)).expect("schedule q3");

        // Releasing q1 lets the cancellation boundary drop it without any
        // emit; the worker must then pick up q3 — never q2.
        release.send(()).expect("release q1");
        assert_eq!(
            started.recv_timeout(Duration::from_secs(5)),
            Ok(3),
            "the newest query runs next; superseded q2 never starts"
        );
        release.send(()).expect("release q3");

        let result = worker
            .results()
            .recv_timeout(Duration::from_secs(5))
            .expect("q3 result");
        assert_eq!(result.context.query_id, QueryId::new(3));
        assert!(result.final_batch);
        assert!(
            matches!(
                worker.results().recv_timeout(Duration::from_millis(200)),
                Err(RecvTimeoutError::Timeout)
            ),
            "superseded queries must not emit batches"
        );
    }
}
