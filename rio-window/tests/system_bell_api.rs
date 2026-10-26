//! Compile-time check that callers can use the bell API on any platform.

use rio_window::window::Window;

const _: fn(&Window) -> bool = Window::request_wayland_system_bell;
