use super::*;
use std::io::Read;
use std::time::{Duration, Instant};

fn shell_output(working_directory: &Option<String>) -> (String, i32) {
    let mut pty = create_pty_with_fork(
        Some("/bin/sh"),
        &[
            "-c".into(),
            "pwd -P; printf '%s\\n' \"$1\" \"$RIO_FORK_CWD_TEST\"".into(),
            "rio-cwd-test".into(),
            "argument with spaces".into(),
        ],
        working_directory,
        &[("RIO_FORK_CWD_TEST".into(), "environment value".into())],
        80,
        24,
        0,
        0,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        assert!(Instant::now() < deadline, "forked PTY output timed out");
        match pty.reader().read(&mut buffer) {
            Ok(0) => break,
            Ok(len) => output.extend_from_slice(&buffer[..len]),
            // Linux PTYs report EIO instead of EOF when the slave closes.
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("failed to read forked PTY: {error}"),
        }
    }
    let status = loop {
        if let Some(status) = pty.child.waitpid().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "forked PTY did not exit");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(libc::WIFEXITED(status));
    (
        String::from_utf8(output).unwrap().replace("\r\n", "\n"),
        libc::WEXITSTATUS(status),
    )
}

#[test]
fn fork_uses_requested_working_directory_without_changing_parent() {
    let parent_directory = std::env::current_dir().unwrap();
    let directory = std::fs::canonicalize(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
    )
    .unwrap();
    let (output, status) = shell_output(&Some(directory.to_str().unwrap().into()));
    assert_eq!(status, 0);
    assert_eq!(
        output,
        format!(
            "{}\nargument with spaces\nenvironment value\n",
            directory.display()
        )
    );
    assert_eq!(std::env::current_dir().unwrap(), parent_directory);
}

#[test]
fn fork_inherits_parent_working_directory_when_unset() {
    let directory = std::env::current_dir().unwrap();
    let (output, status) = shell_output(&None);
    assert_eq!(status, 0);
    assert_eq!(
        output,
        format!(
            "{}\nargument with spaces\nenvironment value\n",
            directory.display()
        )
    );
}

#[test]
fn fork_does_not_run_shell_when_working_directory_is_invalid() {
    let parent_directory = std::env::current_dir().unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for directory in [
        root.join("missing-rio-cwd-test-directory"),
        root.join("Cargo.toml"),
    ] {
        let (output, status) = shell_output(&Some(directory.to_str().unwrap().into()));
        assert_eq!(status, 127);
        assert!(output.is_empty(), "shell ran despite chdir failure");
        assert_eq!(std::env::current_dir().unwrap(), parent_directory);
    }
}

#[test]
fn fork_rejects_working_directory_with_nul_before_forking() {
    let error = create_pty_with_fork(
        Some("/bin/sh"),
        &[],
        &Some("invalid\0directory".into()),
        &[],
        80,
        24,
        0,
        0,
    )
    .err()
    .expect("a directory containing NUL must fail");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}
