//! # Wayland
//!
//! **Note:** Windows don't appear on Wayland until you draw/present to them.
//!
//! By default, Winit loads system libraries using `dlopen`. This can be
//! disabled by disabling the `"wayland-dlopen"` cargo feature.
//!
//! ## Client-side decorations
//!
//! Winit provides client-side decorations by default, but the behaviour can
//! be controlled with the following feature flags:
//!
//! * `wayland-csd-adwaita` (default).
//! * `wayland-csd-adwaita-crossfont`.
//! * `wayland-csd-adwaita-notitle`.
use crate::dpi::LogicalSize;
use crate::event_loop::{ActiveEventLoop, EventLoopBuilder};
use crate::monitor::MonitorHandle;
use crate::window::{Window, WindowAttributes};

pub use crate::window::Theme;

/// Additional methods on [`ActiveEventLoop`] that are specific to Wayland.
pub trait ActiveEventLoopExtWayland {
    /// True if the [`ActiveEventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;
}

impl ActiveEventLoopExtWayland for ActiveEventLoop {
    #[inline]
    fn is_wayland(&self) -> bool {
        self.p.is_wayland()
    }
}

/// Additional methods on [`EventLoopBuilder`] that are specific to Wayland.
pub trait EventLoopBuilderExtWayland {
    /// Force using Wayland.
    fn with_wayland(&mut self) -> &mut Self;

    /// Whether to allow the event loop to be created off of the main thread.
    ///
    /// By default, the window is only allowed to be created on the main
    /// thread, to make platform compatibility easier.
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self;
}

impl<T> EventLoopBuilderExtWayland for EventLoopBuilder<T> {
    #[inline]
    fn with_wayland(&mut self) -> &mut Self {
        self.platform_specific.forced_backend =
            Some(crate::platform_impl::Backend::Wayland);
        self
    }

    #[inline]
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self {
        self.platform_specific.any_thread = any_thread;
        self
    }
}

/// Additional methods on [`Window`] that are specific to Wayland.
pub trait WindowExtWayland {
    /// Whether this window actually uses layer-shell rather than xdg-shell.
    fn is_quake_overlay(&self) -> bool;

    /// Prepare the logical size and output for the next show of a quake overlay.
    ///
    /// Call while hidden, before `set_visible(true)`. With `None`, the compositor
    /// chooses the output anew on show. This is a no-op for ordinary windows.
    fn prepare_quake_overlay(
        &self,
        size: LogicalSize<f64>,
        output: Option<MonitorHandle>,
    );

    /// Request activation using a token supplied by the compositor (for
    /// example, by the GlobalShortcuts portal). This also restores a minimized
    /// window when the compositor grants activation.
    fn activate_with_token(&self, token: String);
}

impl WindowExtWayland for Window {
    fn is_quake_overlay(&self) -> bool {
        match &self.window {
            crate::platform_impl::Window::Wayland(window) => window.is_quake_overlay(),
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => false,
        }
    }

    fn prepare_quake_overlay(
        &self,
        size: LogicalSize<f64>,
        output: Option<MonitorHandle>,
    ) {
        match &self.window {
            crate::platform_impl::Window::Wayland(window) => {
                window.prepare_quake_overlay(size, output.map(|monitor| monitor.inner))
            }
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => {}
        }
    }

    fn activate_with_token(&self, token: String) {
        match &self.window {
            crate::platform_impl::Window::Wayland(window) => {
                window.activate_with_token(token)
            }
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => {}
        }
    }
}

/// Additional methods on [`WindowAttributes`] that are specific to Wayland.
pub trait WindowAttributesExtWayland {
    /// Opt into an undecorated, top-anchored overlay with no workarea reservation,
    /// taskbar entry or workspace association. Falls back to xdg-shell with a
    /// warning if layer-shell is unavailable. Auto-focused windows use exclusive
    /// keyboard focus while mapped; `with_active(false)` uses on-demand focus
    /// (or no keyboard focus on layer-shell versions older than 4).
    fn with_quake_overlay(self, enabled: bool) -> Self;

    /// Build window with the given name.
    ///
    /// The `general` name sets an application ID, which should match the `.desktop`
    /// file distributed with your program. The `instance` is a `no-op`.
    ///
    /// For details about application ID conventions, see the
    /// [Desktop Entry Spec](https://specifications.freedesktop.org/desktop-entry-spec/desktop-entry-spec-latest.html#desktop-file-id)
    fn with_name(self, general: impl Into<String>, instance: impl Into<String>) -> Self;
}

impl WindowAttributesExtWayland for WindowAttributes {
    fn with_quake_overlay(mut self, enabled: bool) -> Self {
        self.platform_specific.quake_overlay = enabled;
        self
    }

    #[inline]
    fn with_name(
        mut self,
        general: impl Into<String>,
        instance: impl Into<String>,
    ) -> Self {
        self.platform_specific.name = Some(crate::platform_impl::ApplicationName::new(
            general.into(),
            instance.into(),
        ));
        self
    }
}

/// Additional methods on `MonitorHandle` that are specific to Wayland.
pub trait MonitorHandleExtWayland {
    /// Returns the inner identifier of the monitor.
    fn native_id(&self) -> u32;
}

impl MonitorHandleExtWayland for MonitorHandle {
    #[inline]
    fn native_id(&self) -> u32 {
        self.inner.native_identifier()
    }
}
