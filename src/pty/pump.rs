use std::{
    io::Read,
    os::{fd::AsRawFd, unix::net::UnixStream},
    thread::{self, JoinHandle},
};

use crossbeam_channel::Sender;
use nix_compat::poll::{PollFd, PollFlags, poll};

use super::{NonblockingBatchReader, ReadCycleState};
use crate::terminal::OutputHandle;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PtyReadEvent {
    Activity { read_cycle: u64, drained: bool },
    Eof,
    Failed(String),
}

pub struct PtyReadPump {
    cancel: Option<UnixStream>,
    join: Option<JoinHandle<()>>,
}

impl PtyReadPump {
    pub fn start(
        mut reader: Box<dyn Read + Send>,
        descriptor: i32,
        output: OutputHandle,
        events: Sender<PtyReadEvent>,
    ) -> crate::Result<Self> {
        let (cancel, cancel_reader) = UnixStream::pair()?;
        let cancel_descriptor = cancel_reader.as_raw_fd();
        let join = thread::Builder::new()
            .name("hokan-pty-read".into())
            .spawn(move || {
                let _cancel_reader = cancel_reader;
                let mut batch_reader = NonblockingBatchReader::default();
                loop {
                    let interests = PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR;
                    let mut descriptors = [
                        PollFd::new(descriptor, interests),
                        PollFd::new(cancel_descriptor, interests),
                    ];
                    match poll(&mut descriptors, -1) {
                        Ok(0) => continue,
                        Ok(_) => {}
                        Err(nix_compat::errno::Errno::EINTR) => continue,
                        Err(error) => {
                            let _ = events.send(PtyReadEvent::Failed(error.to_string()));
                            break;
                        }
                    }

                    // Drain any readable child output before honoring a
                    // cancellation observed in the same poll, so shutdown
                    // still forwards the child's final bytes.
                    if descriptors[0]
                        .revents()
                        .is_none_or(|revents| revents.is_empty())
                    {
                        if descriptors[1]
                            .revents()
                            .is_some_and(|events| !events.is_empty())
                        {
                            break;
                        }
                        continue;
                    }

                    match batch_reader.read_cycle(&mut reader) {
                        Ok(outcome) => {
                            let read_cycle = outcome.read_cycle;
                            let drained = outcome.state == ReadCycleState::DrainedToEagain;
                            for batch in outcome.batches {
                                if let Err(error) = output.child_output(batch) {
                                    let _ = events.send(PtyReadEvent::Failed(error.to_string()));
                                    return;
                                }
                            }
                            let _ = events.send(PtyReadEvent::Activity {
                                read_cycle,
                                drained,
                            });
                            if outcome.state == ReadCycleState::Eof {
                                let _ = events.send(PtyReadEvent::Eof);
                                break;
                            }
                        }
                        Err(error)
                            if error.raw_os_error()
                                == Some(nix_compat::errno::Errno::EIO as i32) =>
                        {
                            let _ = events.send(PtyReadEvent::Eof);
                            break;
                        }
                        Err(error) => {
                            let _ = events.send(PtyReadEvent::Failed(error.to_string()));
                            break;
                        }
                    }
                    if descriptors[1]
                        .revents()
                        .is_some_and(|events| !events.is_empty())
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            cancel: Some(cancel),
            join: Some(join),
        })
    }

    /// Signal the pump to stop and wait for it. Without the cancel signal a
    /// pump could outlive shutdown indefinitely — for example while a
    /// detached grandchild still holds the slave side open — and `join` would
    /// never return.
    pub fn join(mut self) -> crate::Result<()> {
        self.cancel.take();
        self.join_thread()
    }

    fn join_thread(&mut self) -> crate::Result<()> {
        self.join.take().map_or(Ok(()), |join| {
            join.join()
                .map_err(|_| crate::Error::Pty("PTY reader thread panicked".into()))
        })
    }
}

impl Drop for PtyReadPump {
    fn drop(&mut self) {
        self.cancel.take();
        let _ = self.join_thread();
    }
}
