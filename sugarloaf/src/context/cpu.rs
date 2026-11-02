// Copyright (c) 2023-present, Raphael Amorim.
//
// CPU rendering backend context.
//
// Owns a softbuffer surface and exposes its `&mut [u32]` pixel buffer
// directly to the rasterizer. There is no intermediate pixmap or pixel
// format conversion — the rasterizer writes 0x00RRGGBB u32 values straight
// into the buffer that softbuffer presents.

use crate::sugarloaf::{SugarloafWindow, SugarloafWindowSize};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use std::num::NonZeroU32;
use std::rc::Rc;

pub struct SoftbufferHandle {
    window: RawWindowHandle,
    display: RawDisplayHandle,
}

impl raw_window_handle::HasWindowHandle for SoftbufferHandle {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(self.window) })
    }
}

impl raw_window_handle::HasDisplayHandle for SoftbufferHandle {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
    {
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(self.display) })
    }
}

unsafe impl Send for SoftbufferHandle {}
unsafe impl Sync for SoftbufferHandle {}

pub type CpuSurface = softbuffer::Surface<Rc<SoftbufferHandle>, Rc<SoftbufferHandle>>;

pub struct CpuContext {
    pub size: SugarloafWindowSize,
    pub scale: f32,
    /// Buffer width in u32 elements. Row stride equals this.
    pub width_px: u32,
    pub height_px: u32,
    /// `None` while detached (see `detach_surface`).
    pub surface: Option<CpuSurface>,
    handle: Rc<SoftbufferHandle>,
}

impl CpuContext {
    pub fn new(window: SugarloafWindow) -> Self {
        let size = window.size;
        let scale = window.scale;

        let handle = Rc::new(SoftbufferHandle {
            window: window.handle,
            display: window.display,
        });

        let width = (size.width as u32).max(1);
        let height = (size.height as u32).max(1);
        let surface = Self::create_surface(&handle, width, height)
            .unwrap_or_else(|e| panic!("CPU backend: {e}"));

        Self {
            size,
            scale,
            width_px: width,
            height_px: height,
            surface: Some(surface),
            handle,
        }
    }

    fn create_surface(
        handle: &Rc<SoftbufferHandle>,
        width: u32,
        height: u32,
    ) -> Result<CpuSurface, String> {
        let context = softbuffer::Context::new(handle.clone())
            .map_err(|e| format!("failed to create softbuffer context: {e}"))?;
        let mut surface = softbuffer::Surface::new(&context, handle.clone())
            .map_err(|e| format!("failed to create softbuffer surface: {e}"))?;
        if let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) {
            surface
                .resize(w, h)
                .map_err(|e| format!("failed to size softbuffer surface: {e}"))?;
        }
        Ok(surface)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.size.width = width as f32;
        self.size.height = height as f32;
        self.width_px = width;
        self.height_px = height;
        if let (Some(w), Some(h), Some(surface)) = (
            NonZeroU32::new(width),
            NonZeroU32::new(height),
            self.surface.as_mut(),
        ) {
            let _ = surface.resize(w, h);
        }
    }

    #[inline]
    pub fn is_detached(&self) -> bool {
        self.surface.is_none()
    }

    /// Drop the presenter tied to the old window. On Wayland the
    /// softbuffer surface owns `wl_surface`/`wl_buffer`/shm proxies on
    /// the old connection and its `Drop` would call into it; after a
    /// compositor handover that `wl_display` may already be freed, so
    /// the surface is deliberately leaked (a few proxies + shm pool per
    /// handover) instead of dropped. Other window systems drop normally.
    pub fn detach_surface(&mut self) {
        let Some(surface) = self.surface.take() else {
            return;
        };
        if matches!(self.handle.display, RawDisplayHandle::Wayland(_)) {
            tracing::info!("leaking Wayland softbuffer surface of the old display");
            std::mem::forget(surface);
        } else {
            drop(surface);
        }
    }

    /// Create a presenter for `window` and resume rendering. The old
    /// presenter (if still attached) is detached first.
    pub fn attach_surface(
        &mut self,
        window: &SugarloafWindow,
        size: SugarloafWindowSize,
        scale: f32,
    ) -> Result<(), String> {
        self.detach_surface();
        let handle = Rc::new(SoftbufferHandle {
            window: window.handle,
            display: window.display,
        });
        let width = (size.width as u32).max(1);
        let height = (size.height as u32).max(1);
        let surface = Self::create_surface(&handle, width, height)?;
        self.handle = handle;
        self.surface = Some(surface);
        self.size = size;
        self.scale = scale;
        self.width_px = width;
        self.height_px = height;
        Ok(())
    }

    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    pub fn supports_f16(&self) -> bool {
        false
    }
}
