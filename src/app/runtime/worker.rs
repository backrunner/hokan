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
                    let engine = match worker_engine.read() {
                        Ok(engine) => Arc::clone(&engine),
                        Err(_) => break,
                    };
                    let query_id = context.query_id.get();
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
