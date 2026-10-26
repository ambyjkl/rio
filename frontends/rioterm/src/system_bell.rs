//! Linux sound-theme bell. Loading and playback happen off the event loop.
use std::ffi::{c_char, c_int, c_void};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};

pub fn ring() {
    static SENDER: OnceLock<Option<mpsc::SyncSender<()>>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("system-bell".into())
            .spawn(move || worker(receiver))
            .ok()?;
        Some(sender)
    });
    if let Some(sender) = sender {
        // Coalesce bursts rather than blocking the terminal event loop.
        let _ = sender.try_send(());
    }
}

fn worker(receiver: mpsc::Receiver<()>) {
    // SAFETY: The library and context remain on this thread until it exits.
    let Some(bell) = (unsafe { Canberra::open() }) else {
        tracing::debug!("System bell unavailable: libcanberra could not be initialized");
        return;
    };
    let mut last = None::<Instant>;
    while receiver.recv().is_ok() {
        if last.is_some_and(|time| time.elapsed() < Duration::from_millis(100)) {
            continue;
        }
        last = Some(Instant::now());
        // SAFETY: The context and function pointer belong to the loaded library.
        let result = unsafe {
            // Keep at most one outstanding sound even for long sound themes.
            (bell.cancel)(bell.context, 1);
            (bell.play)(
                bell.context,
                1,
                b"event.id\0".as_ptr().cast::<c_char>(),
                b"bell-window-system\0".as_ptr().cast::<c_char>(),
                std::ptr::null::<c_char>(),
            )
        };
        if result < 0 {
            tracing::debug!(result, "System bell playback failed");
        }
    }
}

type Create = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type Destroy = unsafe extern "C" fn(*mut c_void) -> c_int;
type Cancel = unsafe extern "C" fn(*mut c_void, u32) -> c_int;
type Play = unsafe extern "C" fn(*mut c_void, u32, ...) -> c_int;

struct Canberra {
    library: *mut c_void,
    context: *mut c_void,
    destroy: Destroy,
    cancel: Cancel,
    play: Play,
}

impl Canberra {
    unsafe fn open() -> Option<Self> {
        let library = libc::dlopen(
            b"libcanberra.so.0\0".as_ptr().cast(),
            libc::RTLD_LAZY | libc::RTLD_LOCAL,
        );
        if library.is_null() {
            return None;
        }
        let create = libc::dlsym(library, b"ca_context_create\0".as_ptr().cast());
        let destroy = libc::dlsym(library, b"ca_context_destroy\0".as_ptr().cast());
        let cancel = libc::dlsym(library, b"ca_context_cancel\0".as_ptr().cast());
        let play = libc::dlsym(library, b"ca_context_play\0".as_ptr().cast());
        if create.is_null() || destroy.is_null() || cancel.is_null() || play.is_null() {
            libc::dlclose(library);
            return None;
        }
        let create: Create = std::mem::transmute(create);
        let destroy: Destroy = std::mem::transmute(destroy);
        let cancel: Cancel = std::mem::transmute(cancel);
        let play: Play = std::mem::transmute(play);
        let mut context = std::ptr::null_mut();
        if create(&mut context) < 0 || context.is_null() {
            libc::dlclose(library);
            return None;
        }
        Some(Self {
            library,
            context,
            destroy,
            cancel,
            play,
        })
    }
}

impl Drop for Canberra {
    fn drop(&mut self) {
        // SAFETY: The context was created by this library and is destroyed before unloading it.
        unsafe {
            (self.destroy)(self.context);
            libc::dlclose(self.library);
        }
    }
}
