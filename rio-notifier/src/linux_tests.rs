//! These tests use a private bus, never the user's notification server or
//! process-wide DBUS_SESSION_BUS_ADDRESS. Missing dbus-daemon skips the tests.
use super::*;
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc as channel,
    },
};
use zbus::{object_server::SignalEmitter, zvariant::OwnedValue};

const WAIT: Duration = Duration::from_secs(5);

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

struct Bus {
    child: Child,
    directory: PathBuf,
    address: String,
}

impl Bus {
    fn start() -> Option<Self> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let mut directory = std::env::temp_dir();
        if let Ok(session) = std::env::var("PI_SESSION_ID") {
            directory.push(session);
        }
        directory.push(format!(
            "rio-notify-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile", "--print-address=1"])
            .arg(format!("--address=unix:tmpdir={}", directory.display()))
            .stdout(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping isolated D-Bus test: dbus-daemon not installed");
                std::fs::remove_dir_all(directory).unwrap();
                return None;
            }
            Err(error) => panic!("starting private dbus-daemon: {error}"),
        };
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        assert!(!address.is_empty(), "private dbus-daemon failed to start");
        Some(Self {
            child,
            directory,
            address: address.trim().to_owned(),
        })
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

struct MockNotifications {
    next_id: AtomicU32,
    seen: channel::Sender<(u32, String, Vec<String>)>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl MockNotifications {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        _app_name: String,
        _replaces_id: u32,
        _app_icon: String,
        summary: String,
        _body: String,
        actions: Vec<String>,
        _hints: HashMap<String, OwnedValue>,
        _expire_timeout: i32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<u32> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.seen.send((id, summary.clone(), actions)).unwrap();
        if summary == "error" {
            return Err(zbus::fdo::Error::Failed("Notify rejected".into()));
        }
        // All of these signals intentionally precede the Notify reply. This
        // catches subscriptions made too late and KDE token/action ordering bugs.
        let conn = emitter.connection();
        let emit = async |member: &str, notification_id: u32, text: &str| {
            conn.emit_signal(
                None::<&str>,
                PATH,
                DESTINATION,
                member,
                &(notification_id, text),
            )
            .await
            .unwrap();
        };
        if summary == "token-flood" {
            for _ in 0..512 {
                emit("ActionInvoked", id + 1000, "other").await;
            }
        }
        if summary.starts_with("token") {
            emit("ActivationToken", id, &format!("xdg-{id}")).await;
            emit("ActivationToken", id + 1000, "unrelated-token").await;
            emit("ActionInvoked", id + 1000, "default").await;
            emit("ActionInvoked", id, "unrelated-action").await;
            emit("ActionInvoked", id, "default").await;
            emit("ActionInvoked", id, "default").await;
        } else if summary == "no-token" {
            emit("ActivationToken", id + 1000, "unrelated-token").await;
            conn.emit_signal(
                None::<&str>,
                PATH,
                DESTINATION,
                "NotificationClosed",
                &(id + 1000, 2u32),
            )
            .await
            .unwrap();
            emit("ActionInvoked", id, "default").await;
        } else if summary == "closed" {
            emit("ActionInvoked", id, "unrelated-action").await;
            conn.emit_signal(
                None::<&str>,
                PATH,
                DESTINATION,
                "NotificationClosed",
                &(id, 2u32),
            )
            .await
            .unwrap();
            emit("ActionInvoked", id, "default").await;
        }
        Ok(id)
    }
}

struct Fixture {
    server: zbus::blocking::Connection,
    client: Connection,
    seen: channel::Receiver<(u32, String, Vec<String>)>,
    _bus: Bus,
}

impl Fixture {
    fn new() -> Option<Self> {
        let bus = Bus::start()?;
        let (seen, receiver) = channel::channel();
        let server = zbus::blocking::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name(DESTINATION)
            .unwrap()
            .serve_at(
                PATH,
                MockNotifications {
                    next_id: AtomicU32::new(42),
                    seen,
                },
            )
            .unwrap()
            .build()
            .unwrap();
        let client = zbus::blocking::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .method_timeout(WAIT)
            .build()
            .unwrap()
            .into_inner();
        Some(Self {
            server,
            client,
            seen: receiver,
            _bus: bus,
        })
    }

    fn run(
        &self,
        title: &str,
        callback: Option<Activation>,
    ) -> channel::Receiver<zbus::Result<()>> {
        let connection = self.client.clone();
        let request = Request {
            title: title.into(),
            body: "terminal output".into(),
            on_activate: callback,
        };
        let (done, receiver) = channel::channel();
        std::thread::spawn(move || {
            let result = runtime().block_on(async {
                let mut session = Session::new(connection).await?;
                session.send_request(request).await?;
                drain(&mut session).await
            });
            let _ = done.send(result);
        });
        receiver
    }
}

fn callback() -> (Activation, channel::Receiver<Option<String>>) {
    let (sender, receiver) = channel::channel();
    (Box::new(move |token| sender.send(token).unwrap()), receiver)
}

#[test]
fn kde_token_before_action_and_reply_is_delivered_once() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let (callback, activated) = callback();
    let done = fixture.run("token", Some(callback));
    assert_eq!(activated.recv_timeout(WAIT).unwrap(), Some("xdg-42".into()));
    done.recv_timeout(WAIT).unwrap().unwrap();
    assert!(matches!(
        activated.recv_timeout(WAIT),
        Err(channel::RecvTimeoutError::Disconnected)
    ));
    let (_, _, actions) = fixture.seen.recv_timeout(WAIT).unwrap();
    assert_eq!(actions, ["default", "Show terminal"]);
}

#[test]
fn missing_token_and_unrelated_ids_are_handled() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let (callback, activated) = callback();
    let done = fixture.run("no-token", Some(callback));
    assert_eq!(activated.recv_timeout(WAIT).unwrap(), None);
    done.recv_timeout(WAIT).unwrap().unwrap();
}

#[test]
fn close_discards_callback_even_if_action_follows() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let (callback, activated) = callback();
    fixture
        .run("closed", Some(callback))
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert!(matches!(
        activated.recv_timeout(WAIT),
        Err(channel::RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn server_disappearance_discards_callback() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let (callback, activated) = callback();
    let done = fixture.run("linger", Some(callback));
    fixture.seen.recv_timeout(WAIT).unwrap();
    fixture.server.release_name(DESTINATION).unwrap();
    done.recv_timeout(WAIT).unwrap().unwrap();
    assert!(matches!(
        activated.recv_timeout(WAIT),
        Err(channel::RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn delivery_failure_discards_callback() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let (callback, activated) = callback();
    fixture
        .run("error", Some(callback))
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert!(matches!(
        activated.recv_timeout(WAIT),
        Err(channel::RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn legacy_notifications_do_not_request_actions_or_linger() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    fixture
        .run("legacy", None)
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    let (_, _, actions) = fixture.seen.recv_timeout(WAIT).unwrap();
    assert!(actions.is_empty());
}

#[test]
fn public_worker_delivers_click_with_tokio_enabled() {
    const CHILD: &str = "RIO_NOTIFIER_TEST_WORKER";
    if std::env::var_os(CHILD).is_some() {
        let (callback, activated) = callback();
        crate::send_notification_with_activation("token", "terminal output", callback);
        assert_eq!(activated.recv_timeout(WAIT).unwrap(), Some("xdg-42".into()));
        return;
    }
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "platform::tests::public_worker_delivers_click_with_tokio_enabled",
        ])
        .env(CHILD, "1")
        .env("DBUS_SESSION_BUS_ADDRESS", &fixture._bus.address)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "notification worker failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn drain(session: &mut Session) -> zbus::Result<()> {
    while !session.active.is_empty() {
        match session.next_event().await {
            Event::Signal(owner, signal) => session.handle(owner, signal, None),
            Event::OwnerLost(owner) => session.forget_owner(&owner),
            Event::OwnerAcquired => (),
            Event::Disconnected => {
                return Err(zbus::Error::Failure("bus disconnected".into()))
            }
        }
    }
    Ok(())
}

#[test]
fn overlapping_notifications_keep_their_own_tokens() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let connection = fixture.client.clone();
    let (result, receiver) = channel::channel();
    let (done, finished) = channel::channel();
    std::thread::spawn(move || {
        runtime().block_on(async move {
            let mut session = Session::new(connection).await.unwrap();
            for index in 0..16 {
                let result = result.clone();
                let callback =
                    Box::new(move |token| result.send((index, token)).unwrap());
                session
                    .send_request(Request {
                        title: format!("linger-{index}"),
                        body: String::new(),
                        on_activate: Some(callback),
                    })
                    .await
                    .unwrap();
            }
            drain(&mut session).await.unwrap();
        });
        done.send(()).unwrap();
    });
    let mut expected = HashMap::new();
    for _ in 0..16 {
        let (id, title, _) = fixture.seen.recv_timeout(WAIT).unwrap();
        expected.insert(title, id);
    }
    // More than a signal queue's worth of events; the shared subscription must
    // continue draining rather than blocking a Notify reply behind a full queue.
    for (title, id) in &expected {
        for (member, text) in [
            ("ActivationToken", format!("xdg-{id}")),
            ("ActionInvoked", "other".into()),
            ("ActionInvoked", "other".into()),
            ("ActionInvoked", "other".into()),
            ("ActionInvoked", "default".into()),
        ] {
            fixture
                .server
                .emit_signal(None::<&str>, PATH, DESTINATION, member, &(*id, text))
                .unwrap();
        }
        assert!(title.starts_with("linger-"));
    }
    for _ in 0..16 {
        let (index, token) = receiver.recv_timeout(WAIT).unwrap();
        let id = expected.remove(&format!("linger-{index}")).unwrap();
        assert_eq!(token, Some(format!("xdg-{id}")));
    }
    finished.recv_timeout(WAIT).unwrap();
    assert!(expected.is_empty());
}

#[test]
fn signal_flood_before_notify_reply_does_not_deadlock() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let (callback, activated) = callback();
    let done = fixture.run("token-flood", Some(callback));
    assert_eq!(activated.recv_timeout(WAIT).unwrap(), Some("xdg-42".into()));
    done.recv_timeout(WAIT).unwrap().unwrap();
}

#[test]
fn bus_disconnection_discards_callback() {
    let Some(mut fixture) = Fixture::new() else {
        return;
    };
    let (callback, activated) = callback();
    let done = fixture.run("linger", Some(callback));
    fixture.seen.recv_timeout(WAIT).unwrap();
    fixture._bus.child.kill().unwrap();
    assert!(done.recv_timeout(WAIT).unwrap().is_err());
    assert!(matches!(
        activated.recv_timeout(WAIT),
        Err(channel::RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn failed_delivery_does_not_discard_other_notifications() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let (callback, activated) = callback();
    runtime().block_on(async {
        let mut session = Session::new(fixture.client.clone()).await.unwrap();
        session
            .send_request(Request {
                title: "linger".into(),
                body: String::new(),
                on_activate: Some(callback),
            })
            .await
            .unwrap();
        session
            .send_request(Request {
                title: "error".into(),
                body: String::new(),
                on_activate: None,
            })
            .await
            .unwrap();
        assert_eq!(session.active.len(), 1);
        fixture
            .server
            .inner()
            .emit_signal(
                None::<&str>,
                PATH,
                DESTINATION,
                "ActionInvoked",
                &(42u32, "default"),
            )
            .await
            .unwrap();
        drain(&mut session).await.unwrap();
    });
    assert_eq!(activated.recv_timeout(WAIT).unwrap(), None);
}

#[test]
fn callback_retention_is_bounded_but_legacy_delivery_still_works() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    runtime().block_on(async {
        let mut session = Session::new(fixture.client.clone()).await.unwrap();
        for _ in 0..MAX_ACTIVE {
            session
                .send_request(Request {
                    title: "linger".into(),
                    body: String::new(),
                    on_activate: Some(Box::new(|_| {})),
                })
                .await
                .unwrap();
        }
        let (callback, activated) = callback();
        session
            .send_request(Request {
                title: "overflow".into(),
                body: String::new(),
                on_activate: Some(callback),
            })
            .await
            .unwrap();
        assert!(matches!(
            activated.recv_timeout(WAIT),
            Err(channel::RecvTimeoutError::Disconnected)
        ));
        session
            .send_request(Request {
                title: "legacy".into(),
                body: String::new(),
                on_activate: None,
            })
            .await
            .unwrap();
        assert_eq!(session.active.len(), MAX_ACTIVE);
    });
    for _ in 0..MAX_ACTIVE {
        fixture.seen.recv_timeout(WAIT).unwrap();
    }
    let (_, title, actions) = fixture.seen.recv_timeout(WAIT).unwrap();
    assert_eq!(title, "legacy");
    assert!(actions.is_empty());
    assert!(fixture.seen.try_recv().is_err());
}

#[test]
fn reused_id_from_another_server_cannot_activate_old_notification() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let (callback, activated) = callback();
    runtime().block_on(async {
        let mut session = Session::new(fixture.client.clone()).await.unwrap();
        session
            .send_request(Request {
                title: "linger".into(),
                body: String::new(),
                on_activate: Some(callback),
            })
            .await
            .unwrap();
        session.handle(
            ":1.9999".into(),
            NotificationEvent::Action(42, "default".into()),
            None,
        );
        assert_eq!(session.active.len(), 1);
        assert!(matches!(
            activated.try_recv(),
            Err(channel::TryRecvError::Empty)
        ));
        fixture
            .server
            .inner()
            .emit_signal(
                None::<&str>,
                PATH,
                DESTINATION,
                "ActionInvoked",
                &(42u32, "default"),
            )
            .await
            .unwrap();
        drain(&mut session).await.unwrap();
    });
    assert_eq!(activated.recv_timeout(WAIT).unwrap(), None);
}
