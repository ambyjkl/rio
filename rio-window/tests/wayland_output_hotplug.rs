#![cfg(all(target_os = "linux", feature = "wayland"))]

use std::{
    num::NonZeroU32,
    process::Command,
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use rio_window::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    platform::{
        run_on_demand::EventLoopExtRunOnDemand,
        wayland::{
            EventLoopBuilderExtWayland, WindowAttributesExtWayland, WindowExtWayland,
        },
    },
    window::{Window, WindowId},
};

fn sway(command: &str) {
    assert!(
        Command::new("timeout")
            .args(["3", "swaymsg", "-q", command])
            .status()
            .unwrap()
            .success(),
        "sway command failed: {command}"
    );
}

/// Requires RIO_WINDOW_TEST_SWAY_HOTPLUG=1, WAYLAND_DISPLAY and SWAYSOCK for a
/// disposable headless Sway with HEADLESS-1 and HEADLESS-2, both initially enabled.
/// Disabling outputs removes their wl_output globals and closes their layers,
/// exercising the same protocol lifecycle as physically unplugging a monitor.
#[test]
#[ignore = "requires a disposable two-output headless Sway"]
fn quake_overlay_survives_output_hotplug() {
    assert!(std::env::var_os("RIO_WINDOW_TEST_SWAY_HOTPLUG").is_some());

    struct App {
        window: Option<Rc<Window>>,
        surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
        handle: Option<RawWindowHandle>,
        wake: EventLoopProxy<()>,
        reconnected: Option<mpsc::Sender<()>>,
        phase: usize,
        frames: usize,
        previous_frames: usize,
        next_action: Instant,
        deadline: Instant,
    }

    impl App {
        fn window(&self) -> &Window {
            self.window.as_ref().unwrap()
        }
        fn control_flow(&self) -> ControlFlow {
            if self.phase == 3 && !self.rendered_on("HEADLESS-1") {
                // No timer may drive the first automatic reconnect recovery.
                ControlFlow::Wait
            } else {
                ControlFlow::WaitUntil(self.next_action)
            }
        }

        fn rendered_on(&self, name: &str) -> bool {
            self.frames > self.previous_frames
                && self
                    .window()
                    .current_monitor()
                    .and_then(|monitor| monitor.name())
                    .as_deref()
                    == Some(name)
        }
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let names: Vec<_> = event_loop
                .available_monitors()
                .filter_map(|m| m.name())
                .collect();
            assert!(
                names.contains(&"HEADLESS-1".into())
                    && names.contains(&"HEADLESS-2".into())
            );
            sway("output HEADLESS-2 disable");
            let window = Rc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_title("Quake output hotplug regression test")
                            .with_quake_overlay(true)
                            .with_inner_size(LogicalSize::new(320, 240))
                            .with_decorations(false)
                            .with_visible(false),
                    )
                    .unwrap(),
            );
            assert!(window.is_quake_overlay());
            self.handle = Some(window.window_handle().unwrap().as_raw());
            let context = softbuffer::Context::new(window.clone()).unwrap();
            self.surface =
                Some(softbuffer::Surface::new(&context, window.clone()).unwrap());
            let output = event_loop
                .available_monitors()
                .find(|m| m.name().as_deref() == Some("HEADLESS-1"));
            window.prepare_quake_overlay(LogicalSize::new(320., 240.), output);
            window.set_visible(true);
            self.window = Some(window);
        }

        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                    panic!("output removal must not close the terminal: {event:?}");
                }
                WindowEvent::RedrawRequested => {
                    assert_eq!(
                        self.window().is_visible(),
                        Some(true),
                        "hidden quake remapped itself"
                    );
                    let window = self.window.as_ref().unwrap();
                    let size = window.inner_size();
                    assert_eq!(
                        size.to_logical::<u32>(window.scale_factor()),
                        LogicalSize::new(320, 240)
                    );
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
                    self.frames += 1;
                }
                _ => (),
            }
        }

        fn user_event(&mut self, _: &ActiveEventLoop, _: ()) {
            assert!(
                self.phase != 3 || self.rendered_on("HEADLESS-1"),
                "output-only reconnect must render without timers or unrelated input"
            );
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            let now = Instant::now();
            assert!(
                now < self.deadline,
                "hotplug test timed out in phase {}",
                self.phase
            );
            assert_eq!(
                self.window().window_handle().unwrap().as_raw(),
                self.handle.unwrap(),
                "recovery must preserve the rendering surface"
            );
            if now < self.next_action {
                event_loop.set_control_flow(self.control_flow());
                return;
            }
            let outputs = event_loop.available_monitors().count();
            if outputs == 0 {
                assert_eq!(
                    self.window().available_monitors().count(),
                    0,
                    "removed monitors must not survive in the window's cache"
                );
                assert!(self.window().current_monitor().is_none());
            }
            let mut advance = true;
            match self.phase {
                0 if self.rendered_on("HEADLESS-1") => {
                    sway("output HEADLESS-1 disable");
                }
                1 if outputs == 0 => {
                    assert_eq!(self.window().is_visible(), Some(true));
                    self.previous_frames = self.frames;
                    self.window().request_redraw(); // Must not attach an old buffer.
                }
                2 => {
                    assert_eq!(outputs, 0);
                    assert_eq!(self.frames, self.previous_frames);
                    let (reconnected, cancel) = mpsc::channel();
                    self.reconnected = Some(reconnected);
                    let wake = self.wake.clone();
                    thread::spawn(move || {
                        if matches!(
                            cancel.recv_timeout(Duration::from_secs(2)),
                            Err(mpsc::RecvTimeoutError::Timeout)
                        ) {
                            let _ = wake.send_event(());
                        }
                    });
                    sway("output HEADLESS-1 enable");
                }
                3 if self.rendered_on("HEADLESS-1") => {
                    self.reconnected.take().unwrap().send(()).unwrap();
                    self.window().set_visible(false);
                    self.previous_frames = self.frames;
                    sway("output HEADLESS-1 disable");
                }
                4 if outputs == 0 => {
                    sway("output HEADLESS-1 enable");
                }
                5 if outputs == 1 => {
                    assert_eq!(self.window().is_visible(), Some(false));
                    assert_eq!(
                        self.frames, self.previous_frames,
                        "hidden quake must stay hidden after reconnect"
                    );
                    self.window()
                        .prepare_quake_overlay(LogicalSize::new(320., 240.), None);
                    self.window().set_visible(true);
                }
                6 if self.rendered_on("HEADLESS-1") => {
                    sway("output HEADLESS-1 disable");
                }
                7 if outputs == 0 => {
                    self.window().set_visible(false);
                    self.window()
                        .prepare_quake_overlay(LogicalSize::new(320., 240.), None);
                    self.window().set_visible(true);
                    self.window().set_visible(true); // Repeated explicit show while dormant.
                    self.previous_frames = self.frames;
                }
                8 => {
                    assert_eq!(outputs, 0);
                    assert_eq!(self.frames, self.previous_frames);
                    sway("output HEADLESS-1 enable");
                }
                9 if self.rendered_on("HEADLESS-1") => {
                    sway("output HEADLESS-2 enable");
                }
                10 if outputs == 2 => {
                    self.previous_frames = self.frames;
                    sway("output HEADLESS-1 disable");
                }
                11 if outputs == 1 && self.rendered_on("HEADLESS-2") => {
                    sway("output HEADLESS-1 enable");
                }
                12 if outputs == 2 => {
                    self.window().set_visible(false);
                    let output = event_loop
                        .available_monitors()
                        .find(|m| m.name().as_deref() == Some("HEADLESS-1"));
                    assert!(output.is_some());
                    self.window()
                        .prepare_quake_overlay(LogicalSize::new(320., 240.), output);
                    self.previous_frames = self.frames;
                    sway("output HEADLESS-1 disable");
                }
                13 if outputs == 1 => {
                    // The prepared explicit output is now stale. Resolve a live
                    // output at show time instead of sending the removed proxy.
                    self.window().set_visible(true);
                }
                14 if self.rendered_on("HEADLESS-2") => {
                    for _ in 0..3 {
                        self.window().set_visible(false);
                        self.window().set_visible(true);
                    }
                    self.previous_frames = self.frames;
                }
                15 if self.rendered_on("HEADLESS-2") => {
                    event_loop.exit();
                    return;
                }
                _ => advance = false,
            }
            if advance {
                self.phase += 1;
            }
            self.next_action = now + Duration::from_millis(150);
            event_loop.set_control_flow(self.control_flow());
        }
    }

    let mut event_loop = EventLoop::builder()
        .with_wayland()
        .with_any_thread(true)
        .build()
        .unwrap();
    let now = Instant::now();
    let mut app = App {
        window: None,
        surface: None,
        handle: None,
        wake: event_loop.create_proxy(),
        reconnected: None,
        phase: 0,
        frames: 0,
        previous_frames: 0,
        next_action: now + Duration::from_millis(150),
        deadline: now + Duration::from_secs(15),
    };
    event_loop.run_app_on_demand(&mut app).unwrap();
    assert_eq!(app.phase, 15);
}
