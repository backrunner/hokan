use std::{
    path::Path,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, bounded};

use crate::{platform::run_bounded, pty::PtyChild, terminal::OutputHandle};

use super::output_error;

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const PROBE_TIMEOUT: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug)]
struct Request {
    process_group: i32,
    generation: u64,
}

/// Process-name discovery never runs on the input/output thread. One bounded
/// request at a time keeps a busy or missing `ps` from affecting interaction.
pub(super) struct ForegroundTitleProbe {
    requests: Option<Sender<Request>>,
    results: Receiver<(Request, Option<String>)>,
    join: Option<JoinHandle<()>>,
    pending: bool,
    previous_group: Option<i32>,
    next_refresh: Instant,
}

impl ForegroundTitleProbe {
    pub(super) fn start() -> Option<Self> {
        let (requests, receiver) = bounded::<Request>(1);
        let (sender, results) = bounded(1);
        let join = thread::Builder::new()
            .name("hokan-title".into())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let title = process_name(request.process_group);
                    if sender.send((request, title)).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        Some(Self {
            requests: Some(requests),
            results,
            join: Some(join),
            pending: false,
            previous_group: None,
            next_refresh: Instant::now(),
        })
    }

    pub(super) fn tick(
        &mut self,
        enabled: bool,
        pty: &PtyChild,
        output: &OutputHandle,
        now: Instant,
    ) -> crate::Result<()> {
        let group = enabled
            .then(|| pty.foreground_process_group())
            .flatten()
            .filter(|group| *group > 0);
        while let Ok((request, title)) = self.results.try_recv() {
            self.pending = false;
            if group == Some(request.process_group)
                && let Some(title) = title
            {
                output
                    .foreground_title(request.generation, title)
                    .map_err(output_error)?;
            }
        }
        let changed = group != self.previous_group;
        self.previous_group = group;
        if changed {
            self.next_refresh = now;
        }
        if !self.pending
            && now >= self.next_refresh
            && let Some(process_group) = group
        {
            self.next_refresh = now + REFRESH_INTERVAL;
            let state = output.state().map_err(output_error)?;
            if state.foreground
                && let Some(generation) = state.title_generation
            {
                self.pending = self.requests.as_ref().is_some_and(|sender| {
                    sender
                        .try_send(Request {
                            process_group,
                            generation,
                        })
                        .is_ok()
                });
            }
        }
        Ok(())
    }
}

impl Drop for ForegroundTitleProbe {
    fn drop(&mut self) {
        self.requests.take();
        // At most one request is outstanding, so the one-slot results channel
        // can accept it even while shutdown waits for the bounded probe.
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn process_name(pid: i32) -> Option<String> {
    let output = run_bounded(
        "/bin/ps",
        ["-p", &pid.to_string(), "-o", "comm="],
        PROBE_TIMEOUT,
        4096,
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?.trim();
    let name = Path::new(text)
        .file_name()?
        .to_str()?
        .trim_start_matches('-');
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn foreground_probe_reads_only_the_executable_name() {
        // Production retries this best-effort, 150 ms probe on later ticks.
        // A busy parallel test runner may exhaust one attempt; this test
        // checks the discovered name, not the scheduler's latency.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let name = loop {
            if let Some(name) = super::process_name(std::process::id() as i32) {
                break name;
            }
            assert!(std::time::Instant::now() < deadline, "current process");
            std::thread::sleep(super::REFRESH_INTERVAL);
        };
        assert!(!name.contains('/'), "{name:?}");
        assert!(!name.contains("--"), "{name:?}");
    }
}
