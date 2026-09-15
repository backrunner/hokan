use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use nix_compat::fcntl::{FcntlArg, OFlag, fcntl};

use crate::terminal::OutputHandle;

/// How long shutdown may wait for queued child output to flush before forcing
/// the output mailbox closed. A healthy drain takes milliseconds; anything
/// beyond this means a producer or the terminal itself is wedged.
const SHUTDOWN_DRAIN_BUDGET: Duration = Duration::from_secs(3);
/// Bounded wait for the output actor once shutdown was requested, before
/// stdout writes are forced nonblocking.
pub(super) const ACTOR_JOIN_GRACE: Duration = Duration::from_secs(2);
/// Final grace after forcing stdout nonblocking; if the actor still does not
/// exit it is wedged somewhere a write timeout cannot reach and is abandoned.
pub(super) const ACTOR_FORCE_GRACE: Duration = Duration::from_secs(1);

/// Forces shutdown forward when the output drain cannot finish: closing the
/// mailbox wakes any producer parked on a full queue, and asks the actor to
/// restore the terminal at the next opportunity instead of holding teardown
/// hostage to a stalled pipe.
pub(super) struct ShutdownWatchdog {
    disarm: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl ShutdownWatchdog {
    pub(super) fn start(output: OutputHandle) -> Self {
        let disarm = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&disarm);
        let join = thread::Builder::new()
            .name("hokan-shutdown".into())
            .spawn(move || {
                thread::park_timeout(SHUTDOWN_DRAIN_BUDGET);
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                let _ = output.restore_and_exit();
            })
            .ok();
        Self { disarm, join }
    }

    fn disarm_inner(&mut self) {
        self.disarm.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            join.thread().unpark();
            let _ = join.join();
        }
    }

    pub(super) fn disarm(mut self) {
        self.disarm_inner();
    }
}

impl Drop for ShutdownWatchdog {
    fn drop(&mut self) {
        self.disarm_inner();
    }
}

/// `JoinHandle` has no timeout, so the join runs on a helper thread; the
/// caller bounds the wait and may resume waiting after intervention.
pub(super) fn spawn_join_waiter<T: Send + 'static>(
    join: JoinHandle<T>,
) -> Receiver<thread::Result<T>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(join.join());
    });
    receiver
}

/// Switch stdout to nonblocking so a write wedged on a stalled terminal or
/// pipe fails fast. Returns the previous status flags; the caller must pass
/// them to `restore_stdout_flags` because the flag lives on the open file
/// description shared with the process that spawned hokan.
pub(super) fn set_stdout_nonblocking() -> Option<i32> {
    let flags = fcntl(1, FcntlArg::F_GETFL).ok()?;
    fcntl(
        1,
        FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
    )
    .ok()?;
    Some(flags)
}

pub(super) fn restore_stdout_flags(saved: Option<i32>) {
    if let Some(flags) = saved {
        let _ = fcntl(1, FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags)));
    }
}
