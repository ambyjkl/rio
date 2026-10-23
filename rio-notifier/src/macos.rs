use crate::{activate, Activation};
use block2::{Block, RcBlock};
use objc::{class, msg_send, sel, sel_impl};
use objc2::{
    declare_class, msg_send_id,
    rc::{autoreleasepool, Retained},
    runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject},
    ClassType, DeclaredClass,
};
use objc2_foundation::{NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{sync_channel, SyncSender},
        Mutex, Once, OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

// Notification Center history can outlive the process and does not reliably
// report dismissal. Keep only the most recent callbacks, not an unbounded
// history of terminal windows. Notifications evicted here remain displayable.
const MAX_CALLBACKS: usize = 128;
const MAX_QUEUED: usize = 64;

#[derive(Default)]
struct Callbacks(VecDeque<(String, Activation)>);

impl Callbacks {
    fn insert(&mut self, id: String, callback: Activation) -> Option<Activation> {
        let evicted = if self.0.len() >= MAX_CALLBACKS {
            self.0.pop_front().map(|(_, callback)| callback)
        } else {
            None
        };
        self.0.push_back((id, callback));
        evicted
    }

    fn take(&mut self, id: &str) -> Option<Activation> {
        let index = self.0.iter().position(|(key, _)| key == id)?;
        self.0.remove(index).map(|(_, callback)| callback)
    }
}

fn callbacks() -> &'static Mutex<Callbacks> {
    static CALLBACKS: OnceLock<Mutex<Callbacks>> = OnceLock::new();
    CALLBACKS.get_or_init(|| Mutex::new(Callbacks::default()))
}

fn take_callback(id: &str) -> Option<Activation> {
    callbacks()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take(id)
}

declare_class!(
    struct NotificationDelegate;

    // NSObject has no subclassing requirements. All mutable state is in the
    // synchronized Rust registry, not in the Objective-C object.
    unsafe impl ClassType for NotificationDelegate {
        type Super = NSObject;
        type Mutability = objc2::mutability::InteriorMutable;
        const NAME: &'static str = "RioNotifier_UserNotificationCenterDelegate";
    }

    impl DeclaredClass for NotificationDelegate {}

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[allow(deprecated)]
        #[method(userNotificationCenter:willPresentNotification:withCompletionHandler:)]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion: &Block<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            // Installing a delegate must not suppress foreground notifications.
            // Alert also works on macOS versions preceding Banner/List.
            completion.call((
                UNNotificationPresentationOptions::UNNotificationPresentationOptionAlert
                    | UNNotificationPresentationOptions::UNNotificationPresentationOptionSound,
            ));
        }

        #[method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:)]
        fn did_receive_response(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &Block<dyn Fn()>,
        ) {
            let (callback, is_default) = unsafe {
                let id = response.notification().request().identifier().to_string();
                let is_default = &*response.actionIdentifier()
                    == UNNotificationDefaultActionIdentifier;
                (take_callback(&id), is_default)
            };
            // Always acknowledge, including unknown IDs and dismiss actions.
            // Do this before invoking arbitrary user code, and outside locks.
            completion.call(());
            if let Some(callback) = callback {
                if is_default {
                    activate(callback, None);
                }
            }
        }
    }
);

// SAFETY: This stateless NSObject subclass has no thread-affine resources. Its
// delegate methods may be called concurrently; all shared Rust state is locked.
unsafe impl Send for NotificationDelegate {}
unsafe impl Sync for NotificationDelegate {}

// The center's delegate property is weak, so retain our delegate for the
// lifetime of the process, independently of notification requests.
static DELEGATE: OnceLock<Retained<NotificationDelegate>> = OnceLock::new();

unsafe fn ensure_delegate(center: &UNUserNotificationCenter) -> bool {
    // Serialize Rio's authorization and send paths. Never replace an existing
    // foreign delegate (this is not the separate NSApplication delegate).
    static INSTALL: Mutex<()> = Mutex::new(());
    let _install = INSTALL.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(current) = center.delegate() {
        return DELEGATE.get().is_some_and(|ours| {
            std::ptr::eq(&*current, ProtocolObject::from_ref(&**ours))
        });
    }
    let ours = DELEGATE.get_or_init(|| msg_send_id![NotificationDelegate::class(), new]);
    // Class registration/initialization may take time. Recheck ownership after
    // it, rather than relying on the earlier observation of a vacant delegate.
    if let Some(current) = center.delegate() {
        return std::ptr::eq(&*current, ProtocolObject::from_ref(&**ours));
    }
    center.setDelegate(Some(ProtocolObject::from_ref(&**ours)));
    true
}

unsafe fn has_bundle_identifier() -> bool {
    // UNUserNotificationCenter can crash for unbundled `cargo run` processes.
    // Never even obtain the center until this guard has succeeded.
    let bundle: *mut objc::runtime::Object = msg_send![class!(NSBundle), mainBundle];
    if bundle.is_null() {
        return false;
    }
    let identifier: *mut objc::runtime::Object = msg_send![bundle, bundleIdentifier];
    !identifier.is_null()
}

pub(crate) fn request_authorization() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        autoreleasepool(|_| unsafe {
            if !has_bundle_identifier() {
                return;
            }
            let center = UNUserNotificationCenter::currentNotificationCenter();
            ensure_delegate(&center);
            center.requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::UNAuthorizationOptionAlert
                    | UNAuthorizationOptions::UNAuthorizationOptionSound,
                &RcBlock::new(|_ok: Bool, _error: *mut NSError| {}),
            );
        });
    });
}

struct Notification {
    title: String,
    body: String,
    callback: Option<Activation>,
}

pub fn send(title: String, body: String, callback: Option<Activation>) {
    static WORKER: OnceLock<Option<SyncSender<Notification>>> = OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (sender, receiver) = sync_channel::<Notification>(MAX_QUEUED);
        std::thread::Builder::new()
            .name("rio-notifier".into())
            .spawn(move || {
                while let Ok(notification) = receiver.recv() {
                    autoreleasepool(|_| unsafe { notify(notification) });
                }
            })
            .ok()
            .map(|_| sender)
    });
    if let Some(worker) = worker {
        // Do not block the terminal or spawn unbounded threads under a flood.
        let _ = worker.try_send(Notification {
            title,
            body,
            callback,
        });
    }
}

unsafe fn notify(notification: Notification) {
    if !has_bundle_identifier() {
        return;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let owns_delegate = ensure_delegate(&center);

    // Unlike the old fixed identifier, each request has independent history
    // and routes back to the terminal which originated it. Include a process
    // incarnation so persisted notifications cannot match after a restart.
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    static INCARNATION: OnceLock<u128> = OnceLock::new();
    let incarnation = INCARNATION.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    });
    let id = format!(
        "rio-notification-{}-{incarnation}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed),
    );
    if owns_delegate {
        if let Some(callback) = notification.callback {
            let evicted = callbacks()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(id.clone(), callback);
            drop(evicted);
        }
    }
    // With a foreign delegate we still deliver, but drop our callback rather
    // than claiming events which belong to somebody else.
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&notification.title));
    content.setBody(&NSString::from_str(&notification.body));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&id),
        &content,
        None,
    );
    center.addNotificationRequest_withCompletionHandler(
        &request,
        Some(&RcBlock::new(move |error: *mut NSError| {
            if !error.is_null() {
                drop(take_callback(&id));
            }
        })),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn callbacks_are_routed_and_consumed_once() {
        let calls = Arc::new(AtomicU64::new(0));
        let mut entries = Callbacks::default();
        let count = calls.clone();
        entries.insert(
            "first".into(),
            Box::new(move |token| {
                assert!(token.is_none());
                count.fetch_add(1, Ordering::Relaxed);
            }),
        );
        entries.insert("second".into(), Box::new(|_| panic!("wrong notification")));
        assert!(entries.take("unknown").is_none());
        activate(entries.take("first").unwrap(), None);
        assert!(entries.take("first").is_none());
        // Dismissal/failure consumes the callback without activating it.
        drop(entries.take("second"));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(entries.0.is_empty());
    }

    #[test]
    fn retained_callbacks_are_bounded_and_oldest_is_evicted() {
        struct Dropped(Arc<AtomicU64>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let drops = Arc::new(AtomicU64::new(0));
        let mut entries = Callbacks::default();
        for id in 0..=MAX_CALLBACKS {
            let guard = Dropped(drops.clone());
            let evicted = entries.insert(id.to_string(), Box::new(move |_| drop(guard)));
            assert_eq!(evicted.is_some(), id == MAX_CALLBACKS);
        }
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(entries.0.len(), MAX_CALLBACKS);
        assert!(entries.take("0").is_none());
        assert!(entries.take(&MAX_CALLBACKS.to_string()).is_some());
        drop(entries);
        assert_eq!(drops.load(Ordering::Relaxed), (MAX_CALLBACKS + 1) as u64);
    }
}
