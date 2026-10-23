//! One persistent worker and one shared signal subscription. Notifications
//! never retain individual threads, connections, or D-Bus subscriptions.
use crate::{activate, Activation};
use futures::{channel::mpsc, FutureExt, StreamExt};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Mutex, OnceLock},
    time::Duration,
};
use zbus::{Connection, Proxy};

const MAX_PENDING: usize = 64;
const MAX_ACTIVE: usize = 64;
const MAX_EARLY_SIGNALS: usize = 256;
const DESTINATION: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

struct Request {
    title: String,
    body: String,
    on_activate: Option<Activation>,
}

pub fn send(title: String, body: String, on_activate: Option<Activation>) {
    static WORKER: OnceLock<Mutex<mpsc::Sender<Request>>> = OnceLock::new();
    let sender = WORKER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel(MAX_PENDING);
        std::thread::Builder::new()
            .name("rio-notifications".into())
            .spawn(move || {
                // zbus uses Tokio when any workspace consumer (e.g. ashpd)
                // enables it. Its I/O and timeouts need a live runtime here.
                if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    runtime.block_on(worker(receiver));
                }
            })
            .ok();
        Mutex::new(sender)
    });
    if let Ok(mut sender) = sender.lock() {
        let _ = sender.try_send(Request {
            title,
            body,
            on_activate,
        });
    }
}

async fn worker(mut requests: mpsc::Receiver<Request>) {
    while let Some(request) = requests.next().await {
        let Ok(builder) = zbus::connection::Builder::session() else {
            continue;
        };
        let Ok(connection) = builder.method_timeout(Duration::from_secs(5)).build().await
        else {
            continue;
        };
        let Ok(mut session) = Session::new(connection).await else {
            continue;
        };
        if session.send_request(request).await.is_err() {
            continue;
        }
        loop {
            futures::select! {
                request = requests.next().fuse() => {
                    let Some(request) = request else { return };
                    if session.send_request(request).await.is_err() {
                        break;
                    }
                },
                event = session.next_event().fuse() => match event {
                    Event::Signal(owner, signal) => session.handle(owner, signal, None),
                    Event::OwnerLost(owner) => session.forget_owner(&owner),
                    Event::OwnerAcquired => (),
                    Event::Disconnected => break,
                },
            }
        }
    }
}

struct Pending {
    callback: Activation,
    token: Option<String>,
}

struct Session {
    connection: Connection,
    signals: zbus::proxy::SignalStream<'static>,
    owners: zbus::fdo::NameOwnerChangedStream,
    active: HashMap<(String, u32), Pending>,
}

enum Event {
    Signal(String, NotificationEvent),
    OwnerLost(String),
    OwnerAcquired,
    Disconnected,
}

enum NotificationEvent {
    Token(u32, String),
    Action(u32, String),
    Closed(u32),
}

impl Session {
    async fn new(connection: Connection) -> zbus::Result<Self> {
        let proxy = Proxy::new(&connection, DESTINATION, PATH, DESTINATION).await?;
        // Subscribe before any Notify, including all three notification signals.
        let signals = proxy.receive_all_signals().await?;
        let owners = zbus::fdo::DBusProxy::new(&connection)
            .await?
            .receive_name_owner_changed_with_args(&[(0, DESTINATION)])
            .await?;
        Ok(Self {
            connection,
            signals,
            owners,
            active: HashMap::new(),
        })
    }

    async fn next_event(&mut self) -> Event {
        loop {
            futures::select! {
                message = self.signals.next().fuse() => {
                    let Some(message) = message else { return Event::Disconnected };
                    let event = match message.header().member().map(|m| m.as_str()) {
                        Some("ActivationToken") => message.body().deserialize::<(u32, String)>()
                            .ok().map(|(id, token)| NotificationEvent::Token(id, token)),
                        Some("ActionInvoked") => message.body().deserialize::<(u32, String)>()
                            .ok().map(|(id, action)| NotificationEvent::Action(id, action)),
                        Some("NotificationClosed") => message.body().deserialize::<(u32, u32)>()
                            .ok().map(|(id, _)| NotificationEvent::Closed(id)),
                        _ => None,
                    };
                    if let (Some(event), Some(owner)) = (event, message.header().sender()) {
                        return Event::Signal(owner.to_string(), event);
                    }
                },
                change = self.owners.next().fuse() => {
                    let Some(change) = change else { return Event::Disconnected };
                    let Ok(args) = change.args() else { continue };
                    // Initial acquisition may be the service auto-started by
                    // Notify. Losing/replacing an owner invalidates all IDs.
                    return if let Some(owner) = args.old_owner().as_ref() {
                        Event::OwnerLost(owner.to_string())
                    } else {
                        Event::OwnerAcquired
                    };
                },
            }
        }
    }

    fn forget_owner(&mut self, owner: &str) {
        self.active.retain(|(sender, _), _| sender != owner);
    }

    fn handle(
        &mut self,
        owner: String,
        event: NotificationEvent,
        early: Option<&mut VecDeque<(String, NotificationEvent)>>,
    ) {
        let id = match &event {
            NotificationEvent::Token(id, _)
            | NotificationEvent::Action(id, _)
            | NotificationEvent::Closed(id) => *id,
        };
        // IDs are scoped to a server's unique bus name. A restarted server
        // must never activate a callback belonging to its predecessor.
        let key = (owner, id);
        if !self.active.contains_key(&key) {
            // The server may emit a token/action before returning the ID from
            // Notify. Retain a bounded ordered buffer only while a call is in flight.
            if let Some(early) = early {
                if early.len() == MAX_EARLY_SIGNALS {
                    early.pop_front();
                }
                early.push_back((key.0, event));
            }
            return;
        }
        match event {
            NotificationEvent::Token(_, token) => {
                self.active.get_mut(&key).unwrap().token = Some(token)
            }
            NotificationEvent::Action(_, action) if action == "default" => {
                let pending = self.active.remove(&key).unwrap();
                activate(pending.callback, pending.token);
            }
            NotificationEvent::Closed(_) => {
                self.active.remove(&key);
            }
            _ => (),
        }
    }

    async fn send_request(&mut self, request: Request) -> zbus::Result<()> {
        if request.on_activate.is_some() && self.active.len() >= MAX_ACTIVE {
            return Ok(()); // Bound retained callbacks even if notifications never expire.
        }
        let connection = self.connection.clone();
        let proxy = Proxy::new(&connection, DESTINATION, PATH, DESTINATION).await?;
        let hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        let actions: &[&str] = if request.on_activate.is_some() {
            &["default", "Show terminal"]
        } else {
            &[]
        };
        let reply = async {
            let response = proxy
                .call_method(
                    "Notify",
                    &(
                        "Rio",
                        0u32,
                        "rio",
                        &request.title,
                        &request.body,
                        actions,
                        &hints,
                        -1i32,
                    ),
                )
                .await?;
            let id: u32 = response.body().deserialize()?;
            let owner = response
                .header()
                .sender()
                .map(|name| name.to_string())
                .ok_or_else(|| {
                    zbus::Error::Failure("Notify reply has no sender".into())
                })?;
            zbus::Result::Ok((owner, id))
        };
        futures::pin_mut!(reply);
        let mut early = VecDeque::new();
        let key = loop {
            // Continuously drain signals while awaiting the reply. Otherwise a
            // full signal queue can block the connection reader and the reply!
            futures::select! {
                result = reply.as_mut().fuse() => match result {
                    Ok(key) => break key,
                    // A failed delivery must not discard callbacks for other
                    // still-visible notifications on the same connection.
                    Err(_) if !connection.is_closed() => return Ok(()),
                    Err(error) => return Err(error),
                },
                event = self.next_event().fuse() => match event {
                    Event::Signal(owner, event) => self.handle(owner, event, request.on_activate.as_ref().map(|_| &mut early)),
                    Event::OwnerAcquired => (),
                    Event::OwnerLost(owner) => {
                        self.forget_owner(&owner);
                        return Ok(());
                    }
                    Event::Disconnected => return Err(zbus::Error::Failure("notification bus disconnected".into())),
                },
            }
        };
        if let Some(callback) = request.on_activate {
            self.active.insert(
                key,
                Pending {
                    callback,
                    token: None,
                },
            );
            for (owner, event) in early {
                self.handle(owner, event, None);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "linux_tests.rs"]
mod tests;
