use std::{fs, path::Path};

#[test]
fn writable_terminal_handles_are_confined_to_output_and_guard() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut rust_files = Vec::new();
    collect_rust_files(&source_root, &mut rust_files);

    for path in rust_files {
        let relative = path
            .strip_prefix(&source_root)
            .expect("source file is below source root");
        if relative == Path::new("terminal/output.rs") || relative == Path::new("terminal/guard.rs")
        {
            continue;
        }
        let source = fs::read_to_string(&path).expect("Rust source should be readable");
        for forbidden in [
            "stdout()",
            "from_raw_fd",
            "CrosstermBackend<Stdout",
            "CrosstermBackend<io::Stdout",
        ] {
            assert!(
                !source.contains(forbidden),
                "{} contains forbidden writable terminal access `{forbidden}`",
                relative.display()
            );
        }
    }
}

fn collect_rust_files(directory: &Path, output: &mut Vec<std::path::PathBuf>) {
    let entries = fs::read_dir(directory).expect("source directory should be readable");
    for entry in entries {
        let path = entry.expect("source entry should be readable").path();
        if path.is_dir() {
            collect_rust_files(&path, output);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push(path);
        }
    }
}
