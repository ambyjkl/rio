use crate::{activate, Activation};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
};
use windows::{
    core::{IInspectable, HSTRING},
    Data::Xml::Dom::XmlDocument,
    Foundation::TypedEventHandler,
    UI::Notifications::{
        ToastDismissalReason, ToastDismissedEventArgs, ToastFailedEventArgs,
        ToastNotification, ToastNotificationManager,
    },
};

// Keep the native toast alive for its in-process activation events, without
// retaining a thread or introducing reference cycles through the handlers.
struct Pending {
    _toast: ToastNotification,
    callback: Activation,
}

fn pending() -> &'static Mutex<BTreeMap<u64, Pending>> {
    static PENDING: OnceLock<Mutex<BTreeMap<u64, Pending>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn finish(id: u64, activated: bool) {
    let entry = pending()
        .lock()
        .ok()
        .and_then(|mut entries| entries.remove(&id));
    if let Some(entry) = entry {
        if activated {
            activate(entry.callback, None);
        }
    }
}

pub fn send(title: String, body: String, on_activate: Option<Activation>) {
    std::thread::spawn(move || {
        let _ = notify(&title, &body, on_activate);
    });
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn notify(
    title: &str,
    body: &str,
    on_activate: Option<Activation>,
) -> windows::core::Result<()> {
    let xml = XmlDocument::new()?;
    let toast_xml = format!(
        r#"<toast><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual></toast>"#,
        escape_xml(title),
        escape_xml(body),
    );
    xml.LoadXml(&HSTRING::from(&toast_xml))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    let notifier =
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from("Rio"))?;
    if let Some(callback) = on_activate {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
            move |_, _| {
                finish(id, true);
                Ok(())
            },
        ))?;
        toast.Dismissed(&TypedEventHandler::<
            ToastNotification,
            ToastDismissedEventArgs,
        >::new(move |_, args| {
            // A timed-out banner can still be clicked in Action Center.
            let reason = args.as_ref().and_then(|args| args.Reason().ok());
            if reason != Some(ToastDismissalReason::TimedOut) {
                finish(id, false);
            }
            Ok(())
        }))?;
        toast.Failed(
            &TypedEventHandler::<ToastNotification, ToastFailedEventArgs>::new(
                move |_, _| {
                    finish(id, false);
                    Ok(())
                },
            ),
        )?;
        if let Ok(mut entries) = pending().lock() {
            // Notification history may outlive its in-process events. Bound
            // retained callbacks even if Windows never reports dismissal.
            if entries.len() >= 64 {
                entries.pop_first();
            }
            entries.insert(
                id,
                Pending {
                    _toast: toast.clone(),
                    callback,
                },
            );
        }
        if let Err(error) = notifier.Show(&toast) {
            finish(id, false);
            return Err(error);
        }
    } else {
        notifier.Show(&toast)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn escapes_notification_text() {
        assert_eq!(
            super::escape_xml("<Rio & terminal>"),
            "&lt;Rio &amp; terminal&gt;"
        );
    }
}
