use super::*;
use crate::event::VoidListener;
use std::path::Path;
use std::time::Duration;

fn wait_for_directory(
    context: &Context<VoidListener>,
    expected: &Path,
    foreground: bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let actual = if foreground {
            teletypewriter::foreground_process_path(*context.main_fd, context.shell_pid)
        } else {
            teletypewriter::process_working_directory(context.shell_pid)
        }
        .unwrap();
        if actual == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "context directory {actual:?} did not become {expected:?} (foreground={foreground})"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn manager(
    use_fork: bool,
    inherit_current_directory: bool,
    directory: &Path,
    shell_args: Vec<String>,
) -> ContextManager<VoidListener> {
    let config = ContextManagerConfig {
        use_fork,
        cwd: inherit_current_directory,
        working_dir: Some(directory.to_str().unwrap().into()),
        shell: Shell {
            program: Some("/bin/sh".into()),
            args: shell_args,
        },
        spawn_performer: true,
        ..ContextManagerConfig::default()
    };
    ContextManager::start(
        (&Cursor::default(), false),
        VoidListener {},
        WindowId::from(0),
        0,
        0,
        config,
        ContextDimension::default(),
        Margin::default(),
        None,
    )
    .unwrap()
}

fn check_new_tab_directory(use_fork: bool, inherit_current_directory: bool) {
    let parent_directory = std::env::current_dir().unwrap();
    let configured_directory = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let active_directory = configured_directory.join("src");
    let mut manager = manager(
        use_fork,
        inherit_current_directory,
        &active_directory,
        vec!["-c".into(), "exec sleep 30".into()],
    );
    wait_for_directory(manager.current(), &active_directory, false);

    // The configured fallback differs from the active pane's directory.
    manager.config.working_dir = Some(configured_directory.to_str().unwrap().into());
    manager.add_context(true, 1);
    assert_eq!(manager.len(), 2);
    wait_for_directory(
        manager.current(),
        if inherit_current_directory {
            &active_directory
        } else {
            &configured_directory
        },
        false,
    );
    assert_eq!(std::env::current_dir().unwrap(), parent_directory);
}

fn check_foreground_job_directory(use_fork: bool) {
    let parent_directory = std::env::current_dir().unwrap();
    let configured_directory = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let shell_directory = configured_directory.join("src");
    let job_directory = shell_directory.join("context");
    let mut manager = manager(use_fork, true, &configured_directory, vec!["-i".into()]);
    wait_for_directory(manager.current(), &configured_directory, false);

    // A real interactive shell changes its own cwd, then runs a foreground
    // script which changes only the script's cwd. Job control gives the script
    // its own foreground process group, as with /tmp/sh.sh in a terminal.
    manager.current().messenger.send_write(
        format!(
            "cd '{}'\n/bin/sh -c 'cd \"$1\"; exec sleep 30' rio-cwd-test '{}'\n",
            shell_directory.display(),
            job_directory.display(),
        )
        .into_bytes(),
    );
    wait_for_directory(manager.current(), &job_directory, true);
    wait_for_directory(manager.current(), &shell_directory, false);
    assert_ne!(
        unsafe { libc::tcgetpgrp(*manager.current().main_fd) },
        manager.current().shell_pid as libc::pid_t,
        "the foreground script must be distinct from the root shell"
    );

    // Disabling inheritance must still use the configured fallback, even
    // while the foreground job and root shell each have a different cwd.
    manager.config.cwd = false;
    assert_eq!(
        manager.working_directory_for_new_context().as_deref(),
        configured_directory.to_str()
    );
    manager.config.cwd = true;

    // This helper is shared by split creation and both native/non-native tabs.
    let directory = manager.working_directory_for_new_context();
    assert_eq!(directory.as_deref(), shell_directory.to_str());
    let config = ContextManagerConfig {
        working_dir: directory,
        ..manager.config.clone()
    };
    // Exercise the split's PTY creation without requiring a GPU/layout renderer.
    let split_context = ContextManager::create_context(
        (&Cursor::default(), false),
        VoidListener {},
        WindowId::from(0),
        1,
        manager.current().dimension,
        &config,
    )
    .unwrap();
    wait_for_directory(&split_context, &shell_directory, false);
    drop(split_context);

    manager.add_context(true, 2);
    assert_eq!(manager.len(), 2);
    wait_for_directory(manager.current(), &shell_directory, false);
    assert_eq!(std::env::current_dir().unwrap(), parent_directory);
}

#[test]
fn forked_tab_inherits_shell_directory_when_enabled() {
    check_new_tab_directory(true, true);
}

#[test]
fn spawned_tab_inherits_shell_directory_when_enabled() {
    check_new_tab_directory(false, true);
}

#[test]
fn forked_tab_uses_configured_directory_when_inheritance_is_disabled() {
    check_new_tab_directory(true, false);
}

#[test]
fn spawned_tab_uses_configured_directory_when_inheritance_is_disabled() {
    check_new_tab_directory(false, false);
}

#[test]
fn forked_contexts_ignore_foreground_script_directory() {
    check_foreground_job_directory(true);
}

#[test]
fn spawned_contexts_ignore_foreground_script_directory() {
    check_foreground_job_directory(false);
}

#[test]
fn unavailable_shell_directory_uses_configured_fallback() {
    let mut manager =
        ContextManager::start_with_capacity(2, VoidListener {}, WindowId::from(0))
            .unwrap();
    manager.config.cwd = true;
    manager.config.working_dir = Some("configured-directory".into());
    manager.current_mut().shell_pid = i32::MAX as u32;
    assert_eq!(
        manager.working_directory_for_new_context().as_deref(),
        Some("configured-directory")
    );
    manager.config.working_dir = None;
    assert_eq!(manager.working_directory_for_new_context(), None);
}
