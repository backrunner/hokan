use std::{
    ffi::OsStr,
    io::{self, Read},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use wait_timeout::ChildExt;

#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub(crate) fn run_bounded<P, I, S>(
    program: P,
    args: I,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<BoundedOutput, String>
where
    P: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let program = program.as_ref();
    let started = Instant::now();
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child =
        retry_text_file_busy(timeout, || command.spawn()).map_err(|error| error.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "probe stdout was unavailable".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "probe stderr was unavailable".to_owned())?;
    let stdout_join = thread::spawn(move || read_limited(stdout, max_output_bytes));
    let stderr_join = thread::spawn(move || read_limited(stderr, max_output_bytes));

    let status = match child
        .wait_timeout(timeout.saturating_sub(started.elapsed()))
        .map_err(|error| error.to_string())?
    {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_join.join();
            let _ = stderr_join.join();
            return Err(format!(
                "{} timed out after {} ms",
                program.to_string_lossy(),
                timeout.as_millis()
            ));
        }
    };
    let stdout = stdout_join
        .join()
        .map_err(|_| "probe stdout reader panicked".to_owned())??;
    let stderr = stderr_join
        .join()
        .map_err(|_| "probe stderr reader panicked".to_owned())??;
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
    })
}

fn retry_text_file_busy<T>(
    timeout: Duration,
    mut spawn: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let started = Instant::now();
    loop {
        match spawn() {
            // A concurrent fork can briefly retain a writable descriptor for
            // a freshly staged executable until exec closes CLOEXEC files.
            // Linux rejects execution with ETXTBSY during that window. Retry
            // only this transient condition, within the original probe budget.
            Err(error)
                if error.raw_os_error() == Some(nix::errno::Errno::ETXTBSY as i32)
                    && started.elapsed() < timeout =>
            {
                thread::sleep(
                    Duration::from_millis(5).min(timeout.saturating_sub(started.elapsed())),
                );
            }
            result => return result,
        }
    }
}

fn read_limited(mut reader: impl Read, max_bytes: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    reader
        .by_ref()
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > max_bytes {
        return Err(format!("probe output exceeded {max_bytes} bytes"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_text_file_busy_but_not_other_spawn_errors() {
        let mut attempts = 0;
        let value = retry_text_file_busy(Duration::from_secs(1), || {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::from_raw_os_error(
                    nix::errno::Errno::ETXTBSY as i32,
                ))
            } else {
                Ok(42)
            }
        })
        .expect("transient writable descriptor closed");
        assert_eq!(value, 42);
        assert_eq!(attempts, 3);

        attempts = 0;
        let error = retry_text_file_busy(Duration::from_secs(1), || {
            attempts += 1;
            Err::<(), _>(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .expect_err("permanent errors must fail immediately");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(attempts, 1);
    }

    #[test]
    fn persistent_text_file_busy_exhausts_the_probe_budget() {
        let started = Instant::now();
        let timeout = Duration::from_millis(20);
        let error = retry_text_file_busy(timeout, || {
            Err::<(), _>(io::Error::from_raw_os_error(
                nix::errno::Errno::ETXTBSY as i32,
            ))
        })
        .expect_err("busy executable must not retry forever");
        assert_eq!(
            error.raw_os_error(),
            Some(nix::errno::Errno::ETXTBSY as i32)
        );
        assert!(started.elapsed() >= timeout);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn captures_output_and_enforces_timeout() {
        let output = run_bounded("printf", ["bounded"], Duration::from_millis(250), 64)
            .expect("printf probe");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"bounded");

        let error = run_bounded("sleep", ["1"], Duration::from_millis(10), 64)
            .expect_err("sleep should time out");
        assert!(error.contains("timed out"));
    }
}
