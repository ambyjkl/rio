//! System-wide hotkey registration for the quake window. Uses the
//! `global-hotkey` crate: Carbon `RegisterEventHotKey` on macOS (no
//! accessibility permission needed), `RegisterHotKey` on Windows and
//! `XGrabKey` on X11. Wayland uses the XDG Desktop Portal GlobalShortcuts API.

use crate::event::EventProxy;
#[cfg(target_os = "linux")]
use ashpd::desktop::{
    global_shortcuts::{
        BindShortcutsOptions, ConfigureShortcutsOptions, GlobalShortcuts, NewShortcut,
    },
    CreateSessionOptions, Session,
};
use global_hotkey::{hotkey::HotKey, GlobalHotKeyManager};
use rio_backend::event::{RioEvent, RioEventType};

/// Keeps the OS hotkey registrations alive for the app's lifetime.
pub struct GlobalHotkeys {
    _manager: Option<GlobalHotKeyManager>,
    #[cfg(target_os = "linux")]
    portal_stop: Option<futures::channel::oneshot::Sender<()>>,
}

#[cfg(target_os = "linux")]
impl Drop for GlobalHotkeys {
    fn drop(&mut self) {
        if let Some(stop) = self.portal_stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Whether this process may register quake shortcuts or create a quake window.
/// On Linux, keep the D-Bus claim for the process lifetime, independently of
/// hotkey managers recreated on config reload. Other instances neither queue
/// for ownership nor forward toggles. Other platforms retain their behavior.
pub fn owns_quake() -> bool {
    #[cfg(target_os = "linux")]
    {
        static OWNER: std::sync::OnceLock<Option<QuakeOwner>> =
            std::sync::OnceLock::new();
        OWNER
            .get_or_init(QuakeOwner::claim)
            .as_ref()
            .is_some_and(|owner| !owner.connection.is_closed())
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

#[cfg(target_os = "linux")]
struct QuakeOwner {
    connection: ashpd::zbus::Connection,
    // zbus's Tokio tasks must keep running while the name is held.
    _runtime: tokio::runtime::Runtime,
}

#[cfg(target_os = "linux")]
impl QuakeOwner {
    fn claim() -> Option<Self> {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::warn!("could not start quake ownership runtime: {err}");
                return None;
            }
        };
        let result = runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let connection = ashpd::zbus::Connection::session().await?;
                if claim_quake_name(&connection, "com.rioterm.Rio.Quake").await? {
                    Ok::<_, ashpd::zbus::Error>(Some(connection))
                } else {
                    Ok(None)
                }
            })
            .await
        });
        match result {
            Ok(Ok(Some(connection))) => {
                tracing::info!("this Rio owns the quake window");
                Some(Self {
                    connection,
                    _runtime: runtime,
                })
            }
            Ok(Ok(None)) => {
                tracing::info!(
                    "another Rio owns the quake window; skipping quake shortcuts"
                );
                None
            }
            Ok(Err(err)) => {
                tracing::warn!("could not claim quake ownership; quake disabled: {err}");
                None
            }
            Err(_) => {
                tracing::warn!("quake ownership request timed out; quake disabled");
                None
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn claim_quake_name(
    connection: &ashpd::zbus::Connection,
    name: &str,
) -> ashpd::zbus::Result<bool> {
    use ashpd::zbus::fdo::{RequestNameFlags, RequestNameReply};

    // Never replace the first owner and never queue a later instance.
    match connection
        .request_name_with_flags(name, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => Ok(true),
        Ok(_) | Err(ashpd::zbus::Error::NameTaken) => Ok(false),
        Err(err) => Err(err),
    }
}

/// Register a system-wide hotkey for every `ToggleQuake` binding, in the quake
/// owner only. No native hotkey manager is created without valid triggers.
pub fn setup(
    event_proxy: EventProxy,
    keys: &[rio_backend::config::bindings::KeyBinding],
) -> Option<GlobalHotkeys> {
    if !owns_quake() {
        return None;
    }

    let triggers = quake_triggers(keys);
    if triggers.is_empty() {
        return None;
    }

    #[cfg(target_os = "linux")]
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return setup_wayland_portal(event_proxy, &triggers);
    }

    let hotkeys: Vec<(String, HotKey)> = triggers
        .into_iter()
        .filter_map(|trigger| match parse_hotkey(&trigger) {
            Ok(hotkey) => Some((trigger, hotkey)),
            Err(err) => {
                tracing::warn!("quake hotkey '{trigger}': {err}");
                None
            }
        })
        .collect();
    if hotkeys.is_empty() {
        return None;
    }

    let manager = match GlobalHotKeyManager::new() {
        Ok(manager) => manager,
        Err(err) => {
            tracing::warn!("global hotkeys unavailable: {err}");
            return None;
        }
    };

    let mut registered = false;
    for (trigger, hotkey) in hotkeys {
        match manager.register(hotkey) {
            Ok(()) => {
                registered = true;
                tracing::info!("registered global hotkey: {trigger}");
            }
            Err(err) => tracing::warn!("quake hotkey '{trigger}': {err}"),
        }
    }
    if !registered {
        return None;
    }

    // The crate's event receiver is a process-wide channel; one
    // listener serves every manager, including managers recreated on
    // config reload.
    static LISTENER: std::sync::Once = std::sync::Once::new();
    LISTENER.call_once(move || {
        std::thread::spawn(move || {
            use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
            while let Ok(event) = GlobalHotKeyEvent::receiver().recv() {
                if event.state == HotKeyState::Pressed {
                    event_proxy
                        .send_event(RioEventType::Rio(RioEvent::ToggleQuake), unsafe {
                            rio_window::window::WindowId::dummy().into()
                        });
                }
            }
        });
    });

    Some(GlobalHotkeys {
        _manager: Some(manager),
        #[cfg(target_os = "linux")]
        portal_stop: None,
    })
}

// A replacement must not register the same app/action while the previous
// session is still closing: KDE deactivates that action when the old session
// is destroyed. This asynchronous gate never blocks the UI thread.
#[cfg(target_os = "linux")]
static PORTAL_SESSION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(target_os = "linux")]
async fn portal_or_stopped<F: std::future::Future>(
    stopped: &mut futures::channel::oneshot::Receiver<()>,
    operation: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        _ = stopped => None,
        result = operation => Some(result),
    }
}

#[cfg(target_os = "linux")]
struct PortalShortcut {
    id: String,
    preferred_trigger: String,
    hotkey: HotKey,
}

#[cfg(target_os = "linux")]
fn portal_shortcuts(triggers: &[String]) -> Vec<PortalShortcut> {
    triggers
        .iter()
        .enumerate()
        .filter_map(|(i, trigger)| {
            Some(PortalShortcut {
                id: format!("toggle-quake-{i}"),
                preferred_trigger: portal_trigger(trigger)?,
                hotkey: parse_hotkey(trigger).ok()?,
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn setup_wayland_portal(
    event_proxy: EventProxy,
    triggers: &[String],
) -> Option<GlobalHotkeys> {
    let shortcuts = portal_shortcuts(triggers);
    if shortcuts.is_empty() {
        tracing::warn!("no valid ToggleQuake shortcuts for the Wayland portal");
        return None;
    }

    let (stop, mut stopped) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::warn!("could not start Wayland shortcut portal runtime: {err}");
                return;
            }
        };
        // Keep the gate outside block_on so runtime/task teardown also finishes
        // before a replacement starts. Cancelling a queued worker is immediate.
        let Some(session_turn) =
            runtime.block_on(portal_or_stopped(&mut stopped, PORTAL_SESSION.lock()))
        else {
            return;
        };
        runtime.block_on(async move {
            let Some(connection) =
                portal_or_stopped(&mut stopped, ashpd::zbus::Connection::session()).await
            else {
                return;
            };
            let connection = match connection {
                Ok(connection) => connection,
                Err(err) => {
                    tracing::warn!("could not connect to Wayland shortcut portal: {err}");
                    return;
                }
            };
            // Own the connection outside all cancellable portal operations.
            let result =
                run_portal_session(&connection, &mut stopped, &event_proxy, &shortcuts)
                    .await;
            // Explicitly disconnect on every exit, including partial creation
            // errors or failed Close. Do not rely on proxy/runtime destruction.
            if let Err(err) = connection.close().await {
                tracing::warn!("could not disconnect Wayland shortcut portal: {err}");
            }
            if let Err(err) = result {
                tracing::warn!("Wayland global shortcut portal unavailable: {err}");
            }
        });
        drop(runtime);
        drop(session_turn);
    });

    Some(GlobalHotkeys {
        _manager: None,
        portal_stop: Some(stop),
    })
}

#[cfg(target_os = "linux")]
async fn run_portal_session(
    connection: &ashpd::zbus::Connection,
    stopped: &mut futures::channel::oneshot::Receiver<()>,
    event_proxy: &EventProxy,
    shortcuts: &[PortalShortcut],
) -> Result<(), ashpd::Error> {
    let Some(portal) = portal_or_stopped(
        stopped,
        GlobalShortcuts::with_connection(connection.clone()),
    )
    .await
    else {
        return Ok(());
    };
    let portal = portal?;
    if !matches!(stopped.try_recv(), Ok(None)) {
        return Ok(());
    }
    // Once issued, finish CreateSession even if cancelled. Otherwise a remote
    // session could exist without a local handle to explicitly close it.
    let session = portal
        .create_session(CreateSessionOptions::default())
        .await?;
    // Cancel pending BindShortcuts dialogs and output queries too, not only
    // the activation loop. Cleanup itself must not be cancelled.
    let result = portal_or_stopped(
        stopped,
        receive_portal_shortcuts(&portal, &session, event_proxy, shortcuts),
    )
    .await;
    if let Err(err) = session.close().await {
        tracing::warn!("could not close Wayland shortcut portal session: {err}");
    }
    result.unwrap_or(Ok(()))
}

#[cfg(target_os = "linux")]
async fn receive_portal_shortcuts(
    portal: &GlobalShortcuts,
    session: &Session<GlobalShortcuts>,
    event_proxy: &EventProxy,
    shortcuts: &[PortalShortcut],
) -> Result<(), ashpd::Error> {
    use futures::StreamExt;

    let mut activated = portal.receive_activated().await?;
    let new_shortcuts: Vec<_> = shortcuts
        .iter()
        .map(|shortcut| {
            NewShortcut::new(shortcut.id.clone(), "Toggle Rio quake terminal")
                .preferred_trigger(Some(shortcut.preferred_trigger.as_str()))
        })
        .collect();
    let request = portal
        .bind_shortcuts(
            session,
            &new_shortcuts,
            None,
            BindShortcutsOptions::default(),
        )
        .await?;
    let bound = request.response()?;
    for shortcut in bound.shortcuts() {
        if !shortcut.trigger_description().trim().is_empty() {
            tracing::info!(
                "Wayland portal shortcut registered: {} ({})",
                shortcut.id(),
                shortcut.trigger_description(),
            );
        }
    }
    if portal_shortcuts_unassigned(
        bound
            .shortcuts()
            .iter()
            .filter(|shortcut| {
                shortcuts
                    .iter()
                    .any(|configured| configured.id == shortcut.id())
            })
            .map(|shortcut| shortcut.trigger_description()),
    ) {
        // The desktop can remember removed/denied bindings. Request configuration
        // once; never replace a desktop-assigned key with our preferred trigger.
        tracing::warn!("no Wayland quake shortcut is assigned; configure Toggle Rio quake terminal in system shortcut settings");
        if portal.version() >= 2 {
            if let Err(err) = portal
                .configure_shortcuts(session, None, ConfigureShortcutsOptions::default())
                .await
            {
                tracing::warn!("could not open quake shortcut configuration: {err}");
            }
        }
    }
    while let Some(event) = activated.next().await {
        if !shortcuts
            .iter()
            .any(|shortcut| shortcut.id == event.shortcut_id())
        {
            continue;
        }
        let token = event
            .options()
            .get("activation_token")
            .and_then(|value| <&str>::try_from(value).ok())
            .map(str::to_owned);
        let output_name = active_output_name(portal.connection()).await;
        event_proxy.send_event(
            RioEventType::Rio(RioEvent::ToggleQuakeFromPortal {
                activation_token: token,
                output_name,
            }),
            unsafe { rio_window::window::WindowId::dummy().into() },
        );
    }
    Ok(())
}

/// KWin exposes the active output over D-Bus (also used by Yakuake).
/// Standard Wayland does not expose a global cursor position. Query on the
/// portal worker, never the UI thread, and fall back promptly elsewhere.
#[cfg(target_os = "linux")]
async fn active_output_name(connection: &ashpd::zbus::Connection) -> Option<String> {
    tokio::time::timeout(std::time::Duration::from_millis(100), async {
        let proxy =
            ashpd::zbus::Proxy::new(connection, "org.kde.KWin", "/KWin", "org.kde.KWin")
                .await
                .ok()?;
        let name: String = proxy.call("activeOutputName", &()).await.ok()?;
        (!name.is_empty()).then_some(name)
    })
    .await
    .ok()
    .flatten()
}

#[cfg(target_os = "linux")]
fn portal_shortcuts_unassigned<'a>(
    descriptions: impl IntoIterator<Item = &'a str>,
) -> bool {
    descriptions
        .into_iter()
        .all(|description| description.trim().is_empty())
}

#[cfg(target_os = "linux")]
fn portal_trigger(trigger: &str) -> Option<String> {
    let mut modifiers = Vec::new();
    let mut key = None;
    for part in trigger.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "super" | "cmd" | "command" => modifiers.push("<Super>"),
            "ctrl" | "control" => modifiers.push("<Ctrl>"),
            "alt" | "option" => modifiers.push("<Alt>"),
            "shift" => modifiers.push("<Shift>"),
            "none" => {}
            k => {
                key = Some(match k {
                    "escape" | "esc" => "Escape".to_string(),
                    "space" => "space".to_string(),
                    "enter" | "return" => "Return".to_string(),
                    "tab" => "Tab".to_string(),
                    "up" => "Up".to_string(),
                    "down" => "Down".to_string(),
                    "left" => "Left".to_string(),
                    "right" => "Right".to_string(),
                    _ if k.len() == 1 => k.to_ascii_uppercase(),
                    _ if k.starts_with('f') && k[1..].parse::<u8>().is_ok() => {
                        k.to_ascii_uppercase()
                    }
                    _ => return None,
                })
            }
        }
    }
    Some(format!("{}{}", modifiers.join(""), key?))
}

/// Triggers for every `ToggleQuake` binding in the config, in the
/// format `parse_hotkey` accepts.
pub fn quake_triggers(keys: &[rio_backend::config::bindings::KeyBinding]) -> Vec<String> {
    keys.iter()
        .filter(|binding| binding.action.to_lowercase() == "togglequake")
        .map(|binding| {
            let mods = binding.with.replace(' ', "").replace('|', "+");
            if mods.is_empty() {
                binding.key.clone()
            } else {
                format!("{}+{}", mods, binding.key)
            }
        })
        .collect()
}

/// Only effective quake shortcut changes require rebuilding the manager.
/// Ignore unrelated actions and equivalent key/modifier spelling changes.
pub fn quake_bindings_changed(
    previous: &[rio_backend::config::bindings::KeyBinding],
    updated: &[rio_backend::config::bindings::KeyBinding],
) -> bool {
    #[cfg(target_os = "linux")]
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return portal_bindings_changed(previous, updated);
    }
    let hotkeys = |keys: &[rio_backend::config::bindings::KeyBinding]| {
        quake_triggers(keys)
            .into_iter()
            .filter_map(|trigger| parse_hotkey(&trigger).ok())
            .collect::<Vec<_>>()
    };
    hotkeys(previous) != hotkeys(updated)
}

#[cfg(target_os = "linux")]
fn portal_bindings_changed(
    previous: &[rio_backend::config::bindings::KeyBinding],
    updated: &[rio_backend::config::bindings::KeyBinding],
) -> bool {
    // Use exactly the same supported shortcuts and IDs as registration, while
    // comparing canonical chords rather than modifier spelling/order.
    let shortcuts = |keys: &[rio_backend::config::bindings::KeyBinding]| {
        portal_shortcuts(&quake_triggers(keys))
            .into_iter()
            .map(|shortcut| (shortcut.id, shortcut.hotkey))
            .collect::<Vec<_>>()
    };
    shortcuts(previous) != shortcuts(updated)
}

/// Parse a binding-style trigger ("super+shift+q", "f12") into a
/// `HotKey`. Accepts the same modifier names as `[bindings]` `with`.
fn parse_hotkey(trigger: &str) -> Result<HotKey, String> {
    use global_hotkey::hotkey::{Code, Modifiers};

    let lowered = trigger.to_lowercase();
    let mut modifiers = Modifiers::empty();
    let mut code = None;

    for part in lowered.split('+') {
        match part.trim() {
            "cmd" | "super" | "command" => modifiers |= Modifiers::SUPER,
            "ctrl" | "control" => modifiers |= Modifiers::CONTROL,
            "alt" | "option" => modifiers |= Modifiers::ALT,
            "shift" => modifiers |= Modifiers::SHIFT,
            // The bindings parser accepts "none" as a no-op modifier.
            "none" => {}
            "escape" | "esc" => code = Some(Code::Escape),
            "space" => code = Some(Code::Space),
            "enter" | "return" => code = Some(Code::Enter),
            "tab" => code = Some(Code::Tab),
            "back" | "backspace" => code = Some(Code::Backspace),
            "delete" => code = Some(Code::Delete),
            "insert" => code = Some(Code::Insert),
            "home" => code = Some(Code::Home),
            "end" => code = Some(Code::End),
            "pageup" => code = Some(Code::PageUp),
            "pagedown" => code = Some(Code::PageDown),
            "up" => code = Some(Code::ArrowUp),
            "down" => code = Some(Code::ArrowDown),
            "left" => code = Some(Code::ArrowLeft),
            "right" => code = Some(Code::ArrowRight),
            "f1" => code = Some(Code::F1),
            "f2" => code = Some(Code::F2),
            "f3" => code = Some(Code::F3),
            "f4" => code = Some(Code::F4),
            "f5" => code = Some(Code::F5),
            "f6" => code = Some(Code::F6),
            "f7" => code = Some(Code::F7),
            "f8" => code = Some(Code::F8),
            "f9" => code = Some(Code::F9),
            "f10" => code = Some(Code::F10),
            "f11" => code = Some(Code::F11),
            "f12" => code = Some(Code::F12),
            "`" | "grave" | "backquote" => code = Some(Code::Backquote),
            "'" | "quote" => code = Some(Code::Quote),
            "," | "comma" => code = Some(Code::Comma),
            "." | "period" => code = Some(Code::Period),
            "/" | "slash" => code = Some(Code::Slash),
            "\\" | "backslash" => code = Some(Code::Backslash),
            ";" | "semicolon" => code = Some(Code::Semicolon),
            "-" | "minus" => code = Some(Code::Minus),
            "=" | "equal" => code = Some(Code::Equal),
            "[" | "bracketleft" => code = Some(Code::BracketLeft),
            "]" | "bracketright" => code = Some(Code::BracketRight),
            "numpadenter" => code = Some(Code::NumpadEnter),
            "numpadadd" => code = Some(Code::NumpadAdd),
            "numpadsubtract" => code = Some(Code::NumpadSubtract),
            "numpadmultiply" => code = Some(Code::NumpadMultiply),
            "numpaddivide" => code = Some(Code::NumpadDivide),
            "numpaddecimal" => code = Some(Code::NumpadDecimal),
            "numpadcomma" => code = Some(Code::NumpadComma),
            "numpadequals" => code = Some(Code::NumpadEqual),
            "numpad0" => code = Some(Code::Numpad0),
            "numpad1" => code = Some(Code::Numpad1),
            "numpad2" => code = Some(Code::Numpad2),
            "numpad3" => code = Some(Code::Numpad3),
            "numpad4" => code = Some(Code::Numpad4),
            "numpad5" => code = Some(Code::Numpad5),
            "numpad6" => code = Some(Code::Numpad6),
            "numpad7" => code = Some(Code::Numpad7),
            "numpad8" => code = Some(Code::Numpad8),
            "numpad9" => code = Some(Code::Numpad9),
            key if key.len() == 1 => {
                let ch = key.chars().next().unwrap();
                code = Some(match ch {
                    'a'..='z' => letter_code(ch),
                    '0'..='9' => digit_code(ch),
                    _ => return Err(format!("unsupported key '{key}'")),
                });
            }
            other => return Err(format!("unknown key or modifier '{other}'")),
        }
    }

    let code = code.ok_or_else(|| "no key in trigger".to_string())?;
    Ok(HotKey::new(
        (!modifiers.is_empty()).then_some(modifiers),
        code,
    ))
}

fn letter_code(ch: char) -> global_hotkey::hotkey::Code {
    use global_hotkey::hotkey::Code::*;
    match ch {
        'a' => KeyA,
        'b' => KeyB,
        'c' => KeyC,
        'd' => KeyD,
        'e' => KeyE,
        'f' => KeyF,
        'g' => KeyG,
        'h' => KeyH,
        'i' => KeyI,
        'j' => KeyJ,
        'k' => KeyK,
        'l' => KeyL,
        'm' => KeyM,
        'n' => KeyN,
        'o' => KeyO,
        'p' => KeyP,
        'q' => KeyQ,
        'r' => KeyR,
        's' => KeyS,
        't' => KeyT,
        'u' => KeyU,
        'v' => KeyV,
        'w' => KeyW,
        'x' => KeyX,
        'y' => KeyY,
        'z' => KeyZ,
        _ => unreachable!(),
    }
}

fn digit_code(ch: char) -> global_hotkey::hotkey::Code {
    use global_hotkey::hotkey::Code::*;
    match ch {
        '0' => Digit0,
        '1' => Digit1,
        '2' => Digit2,
        '3' => Digit3,
        '4' => Digit4,
        '5' => Digit5,
        '6' => Digit6,
        '7' => Digit7,
        '8' => Digit8,
        '9' => Digit9,
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use global_hotkey::hotkey::{Code, Modifiers};

    fn binding(
        key: &str,
        with: &str,
        action: &str,
    ) -> rio_backend::config::bindings::KeyBinding {
        rio_backend::config::bindings::KeyBinding {
            key: key.into(),
            with: with.into(),
            action: action.into(),
            esc: String::new(),
            mode: String::new(),
        }
    }

    #[test]
    fn unrelated_binding_changes_keep_quake_registration() {
        let previous = vec![
            binding("F12", "", "ToggleQuake"),
            binding("c", "control | shift", "Copy"),
        ];
        let mut updated = previous.clone();
        updated.insert(0, binding("k", "control | shift", "ClearScreenAndHistory"));
        assert!(!quake_bindings_changed(&previous, &updated));

        updated[2] = binding("v", "alt", "Paste");
        updated.reverse();
        assert!(!quake_bindings_changed(&previous, &updated));
        updated.retain(|key| key.action == "ToggleQuake");
        assert!(!quake_bindings_changed(&previous, &updated));
    }

    #[test]
    fn equivalent_quake_bindings_keep_registration() {
        let previous = [binding("F12", "control | shift", "ToggleQuake")];
        let updated = [binding("f12", "SHIFT | ctrl", "togglequake")];
        assert!(!quake_bindings_changed(&previous, &updated));
        assert!(!quake_bindings_changed(
            &[binding("F12", "", "ToggleQuake")],
            &[binding("f12", "none", "ToggleQuake")],
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portal_reload_comparison_matches_supported_registration() {
        let unsupported = [binding("grave", "control", "ToggleQuake")];
        let supported = [binding("`", "ctrl", "ToggleQuake")];
        // Both parse to the same native chord, but only the latter is accepted
        // by the portal trigger parser. It must cause a new registration.
        assert_eq!(parse_hotkey("control+grave"), parse_hotkey("ctrl+`"));
        assert!(portal_shortcuts(&quake_triggers(&unsupported)).is_empty());
        assert_eq!(portal_shortcuts(&quake_triggers(&supported)).len(), 1);
        assert!(portal_bindings_changed(&unsupported, &supported));
    }

    #[test]
    fn quake_trigger_additions_changes_and_removals_update_registration() {
        let previous = [binding("F12", "", "ToggleQuake")];
        for updated in [
            vec![binding("F11", "", "ToggleQuake")],
            vec![binding("F12", "shift", "ToggleQuake")],
            vec![previous[0].clone(), binding("F11", "", "ToggleQuake")],
            vec![binding("F12", "", "Copy")],
            vec![],
        ] {
            assert!(quake_bindings_changed(&previous, &updated));
        }
        assert!(quake_bindings_changed(&[], &previous));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portal_session_handoff_waits_for_old_cleanup() {
        use futures::FutureExt;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let gate = tokio::sync::Mutex::new(());
            let (old_stop, mut old_stopped) = futures::channel::oneshot::channel();
            let old_turn = portal_or_stopped(&mut old_stopped, gate.lock())
                .await
                .unwrap();
            let (_new_stop, mut new_stopped) = futures::channel::oneshot::channel();
            let replacement = portal_or_stopped(&mut new_stopped, gate.lock());
            tokio::pin!(replacement);
            assert!(replacement.as_mut().now_or_never().is_none());

            old_stop.send(()).unwrap();
            old_stopped.await.unwrap();
            // Cancellation is not completion: the old session still owns the
            // gate while waiting for its asynchronous Close acknowledgement.
            assert!(replacement.as_mut().now_or_never().is_none());
            drop(old_turn);
            assert!(replacement.await.is_some());
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancelled_portal_workers_do_not_wait_for_or_create_sessions() {
        use futures::FutureExt;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let gate = tokio::sync::Mutex::new(());
            let _old_turn = gate.lock().await;
            let (stop, mut stopped) = futures::channel::oneshot::channel();
            let queued = portal_or_stopped(&mut stopped, gate.lock());
            tokio::pin!(queued);
            assert!(queued.as_mut().now_or_never().is_none());
            stop.send(()).unwrap();
            assert!(queued.await.is_none());

            let (stop, mut stopped) = futures::channel::oneshot::channel();
            stop.send(()).unwrap();
            let started = std::cell::Cell::new(false);
            assert!(portal_or_stopped(&mut stopped, async { started.set(true) })
                .await
                .is_none());
            assert!(!started.get());
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dropping_hotkeys_cancels_pending_portal_work_immediately() {
        let (stop, mut stopped) = futures::channel::oneshot::channel();
        drop(GlobalHotkeys {
            _manager: None,
            portal_stop: Some(stop),
        });
        assert!(matches!(stopped.try_recv(), Ok(Some(()))));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a session bus; run under dbus-run-session"]
    fn quake_name_has_single_owner() {
        const CHILD: &str = "RIO_TEST_QUAKE_SECONDARY";
        if std::env::var_os(CHILD).is_some() {
            assert!(!owns_quake());
            return;
        }
        assert!(owns_quake());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use ashpd::zbus::{fdo::DBusProxy, Connection};
            use futures::StreamExt;

            let first = Connection::session().await.unwrap();
            let second = Connection::session().await.unwrap();
            let third = Connection::session().await.unwrap();
            let name = "com.rioterm.Rio.Quake.Test";

            assert!(claim_quake_name(&first, name).await.unwrap());
            assert!(claim_quake_name(&first, name).await.unwrap());
            assert!(!claim_quake_name(&second, name).await.unwrap());
            assert!(!claim_quake_name(&third, name).await.unwrap());

            // A denied instance must not silently become the owner when the
            // first exits: it has not registered any global shortcuts.
            let bus = DBusProxy::new(&second).await.unwrap();
            let mut released = bus
                .receive_name_owner_changed_with_args(&[(0, name), (2, "")])
                .await
                .unwrap();
            first.close().await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), released.next())
                .await
                .unwrap()
                .unwrap();
            assert!(!bus.name_has_owner(name.try_into().unwrap()).await.unwrap());
            assert!(claim_quake_name(&third, name).await.unwrap());
            assert!(!claim_quake_name(&second, name).await.unwrap());
        });

        let secondary = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "global_hotkey::tests::quake_name_has_single_owner",
                "--ignored",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            secondary.status.success(),
            "secondary Rio test failed: {}{}",
            String::from_utf8_lossy(&secondary.stdout),
            String::from_utf8_lossy(&secondary.stderr)
        );
        assert!(owns_quake());
    }

    #[test]
    fn parse_hotkey_forms() {
        let hk = parse_hotkey("super+shift+q").unwrap();
        assert_eq!(
            hk,
            HotKey::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyQ)
        );
        let hk = parse_hotkey("control+`").unwrap();
        assert_eq!(hk, HotKey::new(Some(Modifiers::CONTROL), Code::Backquote));
        let hk = parse_hotkey("f12").unwrap();
        assert_eq!(hk, HotKey::new(None, Code::F12));
        assert!(parse_hotkey("super+banana").is_err());
        assert!(parse_hotkey("super+shift").is_err());
        // Parity with the bindings parser.
        let hk = parse_hotkey("none+f12").unwrap();
        assert_eq!(hk, HotKey::new(None, Code::F12));
        let hk = parse_hotkey("control+numpad5").unwrap();
        assert_eq!(hk, HotKey::new(Some(Modifiers::CONTROL), Code::Numpad5));
        let hk = parse_hotkey("shift+insert").unwrap();
        assert_eq!(hk, HotKey::new(Some(Modifiers::SHIFT), Code::Insert));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portal_default_comes_from_config_and_allows_desktop_override() {
        use rio_backend::config::bindings::KeyBinding;

        let bindings = [KeyBinding {
            key: "g".into(),
            with: "control | shift".into(),
            action: "ToggleQuake".into(),
            esc: String::new(),
            mode: String::new(),
        }];
        let triggers = quake_triggers(&bindings);
        assert_eq!(triggers, ["control+shift+g"]);
        assert!(parse_hotkey(&triggers[0]).is_ok());
        assert_eq!(
            portal_trigger(&triggers[0]).as_deref(),
            Some("<Ctrl><Shift>G")
        );

        // A desktop-assigned key need not match Rio's preferred trigger.
        assert!(!portal_shortcuts_unassigned(["Alt+F9"]));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portal_detects_unassigned_shortcuts() {
        assert!(portal_shortcuts_unassigned([]));
        assert!(portal_shortcuts_unassigned([""]));
        assert!(portal_shortcuts_unassigned(["", "  "]));
        assert!(!portal_shortcuts_unassigned(["F12"]));
        assert!(!portal_shortcuts_unassigned(["", "Ctrl+F12"]));
        assert!(!portal_shortcuts_unassigned(["Launch (5)"]));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portal_trigger_uses_xdg_shortcut_syntax() {
        assert_eq!(
            portal_trigger("super+shift+q").as_deref(),
            Some("<Super><Shift>Q")
        );
        assert_eq!(portal_trigger("control+f12").as_deref(), Some("<Ctrl>F12"));
        assert_eq!(portal_trigger("super+banana"), None);
    }

    #[test]
    fn quake_triggers_from_bindings() {
        use rio_backend::config::bindings::KeyBinding;
        let binding = |key: &str, with: &str, action: &str| KeyBinding {
            key: key.into(),
            with: with.into(),
            action: action.into(),
            esc: String::new(),
            mode: String::new(),
        };
        let keys = vec![
            binding("q", "super | shift", "ToggleQuake"),
            binding("f12", "", "togglequake"),
            binding("f12", "none", "togglequake"),
            binding("w", "super", "quit"),
        ];
        let triggers = quake_triggers(&keys);
        assert_eq!(triggers, vec!["super+shift+q", "f12", "none+f12"]);
        for trigger in &triggers {
            assert!(parse_hotkey(trigger).is_ok(), "{trigger} must parse");
        }
    }
}
