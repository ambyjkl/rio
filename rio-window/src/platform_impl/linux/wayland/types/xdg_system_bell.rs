//! Optional compositor system bell.

use wayland_client::globals::{BindError, GlobalList};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Proxy, QueueHandle};
use wayland_protocols::xdg::system_bell::v1::client::xdg_system_bell_v1::XdgSystemBellV1;

use crate::platform_impl::wayland::state::WinitState;

#[derive(Debug)]
pub struct SystemBell(XdgSystemBellV1);

impl SystemBell {
    pub fn bind(
        globals: &GlobalList,
        queue: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        globals.bind(queue, 1..=1, ()).map(Self)
    }

    pub fn ring(&self, surface: &WlSurface) -> bool {
        if !self.0.is_alive() || !surface.is_alive() {
            return false;
        }
        self.0.ring(Some(surface));
        true
    }
}

impl Drop for SystemBell {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

delegate_noop!(WinitState: XdgSystemBellV1);
