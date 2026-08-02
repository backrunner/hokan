use std::thread::{self, JoinHandle};

use crossbeam_channel::Sender;
use signal_hook::{
    consts::signal::{SIGCONT, SIGHUP, SIGINT, SIGTERM, SIGTSTP, SIGWINCH},
    iterator::{Handle, Signals},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalEvent {
    Resize,
    Interrupt,
    Terminate(i32),
    Suspend,
    Continue,
}

pub struct SignalBridge {
    handle: Handle,
    join: Option<JoinHandle<()>>,
}

impl SignalBridge {
    pub fn start(sender: Sender<SignalEvent>) -> crate::Result<Self> {
        let mut signals = Signals::new([SIGWINCH, SIGINT, SIGTERM, SIGHUP, SIGTSTP, SIGCONT])?;
        let handle = signals.handle();
        let join = thread::Builder::new()
            .name("hokann-signals".into())
            .spawn(move || {
                for signal in signals.forever() {
                    let event = match signal {
                        SIGWINCH => SignalEvent::Resize,
                        SIGINT => SignalEvent::Interrupt,
                        SIGTERM | SIGHUP => SignalEvent::Terminate(signal),
                        SIGTSTP => SignalEvent::Suspend,
                        SIGCONT => SignalEvent::Continue,
                        _ => continue,
                    };
                    if sender.send(event).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            handle,
            join: Some(join),
        })
    }
}

impl Drop for SignalBridge {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
