#![cfg(wayland_platform)]

use std::{
    num::NonZeroU32,
    process::Command,
    rc::Rc,
    time::{Duration, Instant},
};

use rio_window::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    platform::{
        run_on_demand::EventLoopExtRunOnDemand,
        wayland::{
            EventLoopBuilderExtWayland, WindowAttributesExtWayland, WindowExtWayland,
        },
    },
    window::{Fullscreen, Window, WindowId},
};

/// Requires a running compositor; use a disposable headless Sway for context tests.
/// RIO_WINDOW_TEST_WINDOWED=1: 50% dimensions instead of 100%.
/// RIO_WINDOW_TEST_OVERLAY=1: layer-shell quake overlay instead of xdg toplevel.
/// RIO_WINDOW_TEST_SWAY_CONTEXT=1: alternate outputs/workspaces on each show.
/// RIO_WINDOW_TEST_EXPLICIT_OUTPUT=1: pass the captured output explicitly.
/// The last option requires SWAYSOCK pointing to a disposable Sway instance.
#[test]
#[ignore = "requires a running Wayland compositor"]
fn hide_and_remap_without_activation_token() {
    struct App {
        window: Option<Rc<Window>>,
        surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
        next_action: Instant,
        deadline: Instant,
        frames: usize,
        hidden: bool,
        awaiting_frame: bool,
        expected_size: LogicalSize<u32>,
        expected_output: Option<String>,
        full_size: bool,
        overlay: bool,
        sway_context: bool,
    }

    impl App {
        fn select_context(&mut self, event_loop: &ActiveEventLoop) {
            let monitors: Vec<_> = event_loop.available_monitors().collect();
            let monitor = &monitors[if self.sway_context {
                self.frames % monitors.len()
            } else {
                0
            }];
            self.expected_size = monitor.size().to_logical(monitor.scale_factor());
            if !self.full_size {
                self.expected_size.width /= 2;
                self.expected_size.height /= 2;
            }
            self.expected_output = monitor.name();
            if self.sway_context {
                assert!(
                    monitors.len() >= 2,
                    "context test needs at least two outputs"
                );
                let command = format!(
                    "focus output {}; workspace quake-test-{}",
                    self.expected_output.as_deref().unwrap(),
                    self.frames
                );
                assert!(Command::new("swaymsg")
                    .args(["-q", &command])
                    .status()
                    .unwrap()
                    .success());
            }
        }

        fn assert_context(&self) {
            if !self.sway_context {
                return;
            }
            let output = Command::new("swaymsg")
                .args(["-t", "get_workspaces", "-r"])
                .output()
                .unwrap();
            assert!(output.status.success());
            let workspaces: serde_json::Value =
                serde_json::from_slice(&output.stdout).unwrap();
            let focused = workspaces
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["focused"] == true)
                .unwrap();
            assert_eq!(
                focused["name"],
                format!("quake-test-{}", self.frames - 1),
                "showing quake must not change workspace"
            );
            assert_eq!(focused["output"].as_str(), self.expected_output.as_deref());
            assert_eq!(
                self.window
                    .as_ref()
                    .unwrap()
                    .current_monitor()
                    .and_then(|m| m.name()),
                self.expected_output,
                "quake must follow the invoking output"
            );
        }
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            self.select_context(event_loop);
            let window = Rc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_title("Wayland visibility regression test")
                            .with_inner_size(self.expected_size)
                            .with_quake_overlay(self.overlay)
                            .with_fullscreen(
                                (self.full_size && !self.overlay)
                                    .then_some(Fullscreen::Borderless(None)),
                            )
                            .with_decorations(false)
                            .with_resizable(false)
                            .with_visible(false),
                    )
                    .unwrap(),
            );
            assert_eq!(
                window.is_quake_overlay(),
                self.overlay,
                "requested layer-shell mode must not silently fall back in this test"
            );
            let context = softbuffer::Context::new(window.clone()).unwrap();
            self.surface =
                Some(softbuffer::Surface::new(&context, window.clone()).unwrap());
            self.window = Some(window);
        }

        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
            if event != WindowEvent::RedrawRequested {
                return;
            }
            assert!(!self.hidden, "hidden windows must not present buffers");
            let window = self.window.as_ref().unwrap();
            assert_eq!(window.is_visible(), Some(true));
            let logical_size: LogicalSize<u32> =
                window.inner_size().to_logical(window.scale_factor());
            let size = window.inner_size();
            let surface = self.surface.as_mut().unwrap();
            surface
                .resize(
                    NonZeroU32::new(size.width).unwrap(),
                    NonZeroU32::new(size.height).unwrap(),
                )
                .unwrap();
            let mut buffer = surface.buffer_mut().unwrap();
            buffer.fill(0x00224466);
            window.pre_present_notify();
            buffer.present().unwrap();
            if self.awaiting_frame && logical_size == self.expected_size {
                self.frames += 1;
                self.awaiting_frame = false;
                self.next_action = Instant::now() + Duration::from_millis(100);
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            assert!(
                Instant::now() < self.deadline,
                "remapped surface never rendered at the requested size"
            );
            if Instant::now() >= self.next_action {
                if self.hidden {
                    self.select_context(event_loop);
                    let window = self.window.as_ref().unwrap();
                    // Deliberately no activation token: not all portals supply one.
                    if self.overlay {
                        // Exercise both compositor selection and the explicit
                        // output captured by the portal's activation handler.
                        let output =
                            if std::env::var_os("RIO_WINDOW_TEST_EXPLICIT_OUTPUT")
                                .is_some()
                            {
                                event_loop
                                    .available_monitors()
                                    .find(|m| m.name() == self.expected_output)
                            } else {
                                None
                            };
                        window.prepare_quake_overlay(self.expected_size.cast(), output);
                    } else if self.full_size {
                        window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                    } else {
                        window.set_fullscreen(None);
                        window.set_resizable(true);
                        window.set_min_inner_size(None::<LogicalSize<u32>>);
                        window.set_max_inner_size(None::<LogicalSize<u32>>);
                        let _ = window.request_inner_size(self.expected_size);
                        window.set_min_inner_size(Some(self.expected_size));
                        window.set_max_inner_size(Some(self.expected_size));
                        window.set_resizable(false);
                    }
                    window.set_visible(true);
                    self.hidden = false;
                    self.awaiting_frame = true;
                } else if !self.awaiting_frame {
                    let window = self.window.as_ref().unwrap();
                    assert_eq!(
                        window.inner_size().to_logical::<u32>(window.scale_factor()),
                        self.expected_size
                    );
                    self.assert_context();
                    if self.frames >= 4 {
                        event_loop.exit();
                        return;
                    }
                    window.set_visible(false);
                    self.hidden = true;
                    window.request_redraw(); // Must not accidentally remap it.
                }
                assert_eq!(
                    self.window.as_ref().unwrap().is_visible(),
                    Some(!self.hidden)
                );
                self.next_action = Instant::now() + Duration::from_millis(100);
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                self.next_action.min(self.deadline),
            ));
        }
    }

    let mut event_loop = EventLoop::builder()
        .with_wayland()
        .with_any_thread(true)
        .build()
        .unwrap();
    let mut app = App {
        window: None,
        surface: None,
        next_action: Instant::now() + Duration::from_millis(100),
        deadline: Instant::now() + Duration::from_secs(8),
        frames: 0,
        hidden: true,
        awaiting_frame: true,
        expected_size: LogicalSize::new(0, 0),
        expected_output: None,
        full_size: std::env::var_os("RIO_WINDOW_TEST_WINDOWED").is_none(),
        overlay: std::env::var_os("RIO_WINDOW_TEST_OVERLAY").is_some(),
        sway_context: std::env::var_os("RIO_WINDOW_TEST_SWAY_CONTEXT").is_some(),
    };
    assert!(
        !app.sway_context || app.overlay,
        "context test requires overlay mode"
    );
    event_loop.run_app_on_demand(&mut app).unwrap();
    assert!(
        app.frames >= 4,
        "expected a frame after each of three remaps"
    );
}
