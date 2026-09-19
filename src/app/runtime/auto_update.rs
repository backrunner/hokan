use std::{path::PathBuf, time::Instant};

use crate::config::UpdateConfig;

// Shared on-disk caches decide when network access is due. Revisit them once
// a minute so another session's recent check cannot delay this session by a
// second full interval when it gets a cached result.
const CACHE_POLL_SECS: u64 = 60;

/// Keep the installation path captured before an atomic replacement. On Linux,
/// current_exe() in an old session can later point at a deleted inode.
pub(super) struct AutoUpdateScheduler {
    exe: Option<PathBuf>,
    last: Option<(Instant, UpdateConfig)>,
    opted_out: bool,
}

impl AutoUpdateScheduler {
    pub(super) fn new() -> Self {
        Self {
            exe: std::env::current_exe().ok(),
            last: None,
            opted_out: std::env::var_os("HOKAN_NO_AUTO_UPDATE").is_some(),
        }
    }

    pub(super) fn tick(&mut self, config: &UpdateConfig, now: Instant) {
        if self.due(config, now) {
            self.last = Some((now, config.clone()));
            if let Some(exe) = &self.exe {
                spawn(exe);
            }
        }
    }

    fn due(&mut self, config: &UpdateConfig, now: Instant) -> bool {
        if self.opted_out || !config.enabled {
            self.last = None;
            return false;
        }
        self.last.as_ref().is_none_or(|(last, previous)| {
            previous != config
                || now.duration_since(*last).as_secs() >= config.interval_secs.min(CACHE_POLL_SECS)
        })
    }
}

fn spawn(exe: &std::path::Path) {
    if let Ok(mut child) = update_command(exe).spawn() {
        let _ = std::thread::Builder::new()
            .name("hokan-auto-update".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
}

fn update_command(exe: &std::path::Path) -> std::process::Command {
    use std::{
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };

    // A separate process group avoids Ctrl-C/SIGHUP sent to the terminal's
    // foreground group. Redirecting stdio alone does not provide that isolation.
    let mut command = Command::new(exe);
    command
        .args(["upgrade", "--auto"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn checks_at_start_and_during_long_sessions_and_applies_live_preferences() {
        let now = Instant::now();
        let mut scheduler = AutoUpdateScheduler {
            exe: None,
            last: None,
            opted_out: false,
        };
        let mut config = UpdateConfig::default();
        scheduler.tick(&config, now);
        assert!(!scheduler.due(&config, now + Duration::from_secs(59)));
        assert!(scheduler.due(&config, now + Duration::from_secs(60)));
        scheduler.tick(&config, now + Duration::from_secs(60));
        assert!(!scheduler.due(&config, now + Duration::from_secs(61)));
        config.interval_secs = 600;
        assert!(scheduler.due(&config, now + Duration::from_secs(61)));
        scheduler.tick(&config, now + Duration::from_secs(61));
        config.enabled = false;
        assert!(!scheduler.due(&config, now + Duration::from_secs(3600)));
        config.enabled = true;
        assert!(scheduler.due(&config, now + Duration::from_secs(3600)));
        scheduler.opted_out = true;
        assert!(!scheduler.due(&config, now + Duration::from_secs(3600)));
    }

    #[test]
    fn updater_has_its_own_process_group() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("root");
        let exe = root.path().join("updater");
        std::fs::write(&exe, "#!/bin/sh\nexec sleep 30\n").expect("updater fixture");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).expect("mode");
        let mut child = update_command(&exe).spawn().expect("updater");
        let pid = nix::unistd::Pid::from_raw(child.id() as i32);
        let group = nix::unistd::getpgid(Some(pid));
        child.kill().expect("stop fixture");
        child.wait().expect("reap fixture");
        assert_eq!(group.expect("child group"), pid);
        assert_ne!(pid, nix::unistd::getpgrp());
    }
}
