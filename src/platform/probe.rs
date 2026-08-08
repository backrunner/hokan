use std::{
    ffi::OsStr,
    io::Read,
    process::{Command, Stdio},
    thread,
    time::Duration,
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
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
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
        .wait_timeout(timeout)
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
