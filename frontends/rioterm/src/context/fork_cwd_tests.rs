use super::*;
use crate::event::VoidListener;
use std::path::Path;
use std::time::Duration;

fn wait_for_directory(context: &Context<VoidListener>, expected: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let actual =
            teletypewriter::foreground_process_path(*context.main_fd, context.shell_pid)
                .unwrap();
        if actual == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "forked context directory {actual:?} did not become {expected:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn check_new_tab_directory(inherit_current_directory: bool) {
    let parent_directory = std::env::current_dir().unwrap();
    let configured_directory = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let active_directory = configured_directory.join("src");
    let config = ContextManagerConfig {
        use_fork: true,
        cwd: inherit_current_directory,
        working_dir: Some(active_directory.to_str().unwrap().into()),
        shell: Shell {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), "exec sleep 30".into()],
        },
        spawn_performer: true,
        ..ContextManagerConfig::default()
    };
    let mut manager = ContextManager::start(
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
    .unwrap();
    wait_for_directory(manager.current(), &active_directory);

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
    );
    assert_eq!(std::env::current_dir().unwrap(), parent_directory);
}

#[test]
fn forked_tab_inherits_active_directory_when_enabled() {
    check_new_tab_directory(true);
}

#[test]
fn forked_tab_uses_configured_directory_when_inheritance_is_disabled() {
    check_new_tab_directory(false);
}
