use std::{
    panic::{self, AssertUnwindSafe},
    process::ExitCode,
};

use clap::Parser;

fn main() -> ExitCode {
    let cli = hokann::cli::Cli::parse();
    let mut output = Vec::new();
    match hokann::cli::run(cli, &mut output) {
        Ok(Some(session)) => {
            match run_with_session_panic_guard(|| hokann::app::run_session(session)) {
                Ok(Ok(code)) => ExitCode::from(code),
                Ok(Err(error)) => report_session_failure(&error.to_string()),
                Err(()) => report_session_failure("internal panic during the terminal session"),
            }
        }
        Ok(None) => match hokann::terminal::write_process_output(&output) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("hokann: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!("hokann: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_with_session_panic_guard<T>(operation: impl FnOnce() -> T) -> Result<T, ()> {
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let result = panic::catch_unwind(AssertUnwindSafe(operation));
    panic::set_hook(previous_hook);
    result.map_err(|_| ())
}

fn report_session_failure(message: &str) -> ExitCode {
    eprintln!("hokann: {message}");
    eprintln!("hokann: if terminal input or echo is broken, run: stty sane");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_panic_is_contained_without_rendering_its_payload() {
        let result = run_with_session_panic_guard(|| panic!("credential-like panic payload"));
        assert_eq!(result, Err(()));
    }
}
