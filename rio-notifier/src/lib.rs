/// Request notification authorization from the OS.
/// On macOS this triggers the permission prompt on first call.
/// No-op on other platforms.
pub fn request_authorization() {
    #[cfg(target_os = "macos")]
    platform::request_authorization();
}

type Activation = Box<dyn FnOnce(Option<String>) + Send + 'static>;

/// Send a desktop notification using the platform's native API.
///
/// The caller is never blocked. Use [`send_notification_with_activation`] to
/// associate a notification click with its originating terminal window.
pub fn send_notification(title: &str, body: &str) {
    send(title, body, None);
}

/// Send a desktop notification and handle its default (click) action once.
///
/// On Linux, the callback receives the compositor's activation token, when the
/// notification server supplies one (notably on Wayland). Without a token,
/// compositor focus-stealing prevention may refuse to focus the window. Servers
/// which do not support actions cannot report clicks. On Windows and macOS the
/// token is always `None`. macOS requires an app bundle and an available
/// notification-center delegate; an existing foreign delegate is never replaced.
///
/// The callback may run off the UI thread and should return promptly, e.g. by
/// sending an event to the UI thread. It is dropped without being called if the
/// notification closes, delivery fails, or activation is unsupported. Linux and
/// macOS use bounded workers: excess requests are dropped rather than creating
/// unbounded threads. macOS also bounds retained callbacks; older notifications
/// may remain visible without supporting activation.
pub fn send_notification_with_activation(
    title: &str,
    body: &str,
    on_activate: impl FnOnce(Option<String>) + Send + 'static,
) {
    send(title, body, Some(Box::new(on_activate)));
}

fn send(title: &str, body: &str, on_activate: Option<Activation>) {
    let title = if title.is_empty() { "Rio" } else { title }.to_owned();
    let body = body.to_owned();
    platform::send(title, body, on_activate);
}

// User callbacks must not unwind through native event handlers or kill the
// persistent notification worker.
fn activate(callback: Activation, token: Option<String>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(token)));
}

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
#[path = "linux.rs"]
mod platform;

#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod platform;
