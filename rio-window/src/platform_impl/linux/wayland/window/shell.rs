//! Shell roles with a stable underlying surface.
//!
//! A layer role is recreated on every show so the requested output (or
//! compositor choice) is resolved again.
//! wl_surface permits assigning its *current* role again; layer-shell does not
//! forbid this once the old role object has been destroyed and the buffer
//! detached. This is not a transition between xdg-shell and layer-shell roles.
//!
//! SCTK 0.19's LayerSurface owns and destroys its wl_surface on drop. Own the
//! layer protocol object separately instead, so EGL/Vulkan/raw handles survive.

use std::sync::{Arc, Mutex};

use sctk::globals::ProvidesBoundGlobal;
use sctk::reexports::client::protocol::wl_output::WlOutput;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Proxy, QueueHandle};
use sctk::reexports::protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};
use sctk::shell::wlr_layer::LayerShell;
use sctk::shell::xdg::window::Window;
use sctk::shell::WaylandSurface;

use crate::dpi::LogicalSize;
use crate::platform_impl::wayland::output::MonitorHandle;
use crate::platform_impl::wayland::state::WinitState;

#[derive(Clone, Debug)]
pub enum ShellWindow {
    Xdg(Window),
    Layer(Arc<LayerWindow>),
}

impl ShellWindow {
    pub fn xdg(&self) -> Option<&Window> {
        match self {
            Self::Xdg(window) => Some(window),
            Self::Layer(_) => None,
        }
    }

    pub fn layer(&self) -> Option<&LayerWindow> {
        match self {
            Self::Layer(window) => Some(window),
            Self::Xdg(_) => None,
        }
    }
}

impl WaylandSurface for ShellWindow {
    fn wl_surface(&self) -> &WlSurface {
        match self {
            Self::Xdg(window) => window.wl_surface(),
            Self::Layer(window) => &window.surface,
        }
    }
}

#[derive(Debug)]
pub struct LayerWindow {
    surface: WlSurface,
    shell: ZwlrLayerShellV1,
    namespace: String,
    focus: bool,
    state: Mutex<LayerState>,
}

#[derive(Debug)]
struct LayerState {
    role: Option<ZwlrLayerSurfaceV1>,
    size: LogicalSize<u32>,
    output: Option<WlOutput>,
}

impl LayerWindow {
    pub fn new(
        surface: WlSurface,
        shell: &LayerShell,
        namespace: String,
        size: LogicalSize<u32>,
        focus: bool,
        visible: bool,
        qh: &QueueHandle<WinitState>,
    ) -> Self {
        let shell =
            <LayerShell as ProvidesBoundGlobal<ZwlrLayerShellV1, 1>>::bound_global(shell)
                .expect("layer shell was bound");
        let window = Self {
            surface,
            shell,
            namespace,
            focus,
            state: Mutex::new(LayerState {
                role: None,
                size,
                output: None,
            }),
        };
        window.recreate(visible, qh, &[]);
        window
    }

    /// Called only while unmapped, before the bufferless initial commit.
    pub fn recreate(
        &self,
        visible: bool,
        qh: &QueueHandle<WinitState>,
        monitors: &[MonitorHandle],
    ) {
        let mut state = self.state.lock().unwrap();
        if let Some(role) = state.role.take() {
            role.destroy();
        }
        // An explicit output is a one-show request, not a sticky association.
        // A removed global's proxy can still be alive. Validate against the
        // current outputs, not Proxy::is_alive(), at the actual point of use.
        let output = state
            .output
            .take()
            .filter(|output| monitors.iter().any(|monitor| monitor.proxy == *output));
        let role = self.shell.get_layer_surface(
            &self.surface,
            output.as_ref(),
            Layer::Overlay,
            self.namespace.clone(),
            qh,
            self.surface.clone(),
        );
        role.set_anchor(Anchor::Top);
        role.set_exclusive_zone(-1);
        role.set_size(state.size.width.max(1), state.size.height.max(1));
        // On-demand focus does not guarantee activation of layer surfaces, even
        // on compositors supporting xdg-activation. Use exclusive focus for an
        // auto-focused quake window, but never for an initially hidden one.
        let keyboard = if !visible {
            KeyboardInteractivity::None
        } else if self.focus {
            KeyboardInteractivity::Exclusive
        } else if role.version() >= 4 {
            KeyboardInteractivity::OnDemand
        } else {
            KeyboardInteractivity::None
        };
        role.set_keyboard_interactivity(keyboard);
        state.role = Some(role);
    }

    pub fn set_output(&self, output: Option<WlOutput>) {
        self.state.lock().unwrap().output = output;
    }

    pub fn set_size(&self, size: LogicalSize<u32>) {
        let mut state = self.state.lock().unwrap();
        state.size = LogicalSize::new(size.width.max(1), size.height.max(1));
        if let Some(role) = &state.role {
            role.set_size(state.size.width, state.size.height);
        }
    }

    pub fn is_current(&self, role: &ZwlrLayerSurfaceV1) -> bool {
        self.state.lock().unwrap().role.as_ref() == Some(role)
    }

    pub fn destroy_role(&self) {
        if let Some(role) = self.state.lock().unwrap().role.take() {
            role.destroy();
        }
    }
}

impl Drop for LayerWindow {
    fn drop(&mut self) {
        self.destroy_role();
        self.surface.destroy();
    }
}
