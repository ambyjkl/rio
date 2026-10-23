use super::*;
use rio_backend::crosswords::pos::{Column, Line, Pos};
use rio_window::platform::{
    run_on_demand::EventLoopExtRunOnDemand, wayland::EventLoopBuilderExtWayland,
};
use std::process::Command;
use wayland_client::{
    delegate_noop,
    protocol::{wl_pointer, wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

const TEXT: &str = "RIO_PRIMARY_SELECTION";
const CLIPBOARD_SENTINEL: &str = "regular clipboard must stay unchanged";

#[derive(Default)]
struct InputState {
    manager: Option<ZwlrVirtualPointerManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    has_keyboard: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for InputState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "zwlr_virtual_pointer_manager_v1" => {
                    state.manager = Some(registry.bind(name, 1, queue, ()));
                }
                "wl_seat" => state.seat = Some(registry.bind(name, 1, queue, ())),
                _ => (),
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for InputState {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: wayland_client::WEnum::Value(capabilities),
        } = event
        {
            state.has_keyboard = capabilities.contains(wl_seat::Capability::Keyboard);
        }
    }
}

delegate_noop!(InputState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(InputState: ignore ZwlrVirtualPointerV1);

struct Pointer {
    queue: EventQueue<InputState>,
    state: InputState,
    pointer: ZwlrVirtualPointerV1,
    started: Instant,
}

impl Pointer {
    fn new() -> Self {
        let connection = Connection::connect_to_env().unwrap();
        let mut queue = connection.new_event_queue();
        let handle = queue.handle();
        connection.display().get_registry(&handle, ());
        let mut state = InputState::default();
        queue.roundtrip(&mut state).unwrap();
        let pointer = state
            .manager
            .as_ref()
            .expect("compositor must support virtual pointers")
            .create_virtual_pointer(state.seat.as_ref(), &handle, ());
        queue.roundtrip(&mut state).unwrap();
        assert!(
            state.has_keyboard,
            "test requires a compositor with a keyboard seat"
        );
        Self {
            queue,
            state,
            pointer,
            started: Instant::now(),
        }
    }

    fn move_to(&mut self, x: u32, y: u32, width: u32, height: u32) {
        self.pointer.motion_absolute(
            self.started.elapsed().as_millis() as u32,
            x,
            y,
            width,
            height,
        );
        self.frame();
    }

    fn button(&mut self, button: u32, state: wl_pointer::ButtonState) {
        self.pointer
            .button(self.started.elapsed().as_millis() as u32, button, state);
        self.frame();
    }

    fn frame(&mut self) {
        self.pointer.frame();
        self.queue.roundtrip(&mut self.state).unwrap();
    }
}

fn store_external(primary: bool, text: &str) {
    let mut command = Command::new("timeout");
    command.args(["2", "wl-copy"]);
    if primary {
        command.arg("--primary");
    }
    assert!(command.arg(text).status().unwrap().success());
}

fn load_external(primary: bool) -> String {
    let mut command = Command::new("timeout");
    command.args(["2", "wl-paste", "--no-newline"]);
    if primary {
        command.arg("--primary");
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "wl-paste failed: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// Set RIO_PRIMARY_SELECTION_WAYLAND_TEST=1 and RIO_CONFIG_HOME to a disposable
/// directory. Requires a disposable compositor with one output, a keyboard
/// seat, the virtual pointer and PRIMARY protocols (e.g. headless Sway), and
/// wl-clipboard.
/// Real drag events and another Wayland client verify compositor ownership,
/// not just the contents cached by Rio's own clipboard provider.
#[test]
#[ignore = "requires a disposable Wayland compositor and wl-clipboard"]
fn mouse_selection_claims_wayland_primary_independently_of_copy_on_select() {
    assert!(std::env::var_os("RIO_PRIMARY_SELECTION_WAYLAND_TEST").is_some());
    struct TestApp<'a> {
        app: Application<'a>,
        input: Pointer,
        next_action: Instant,
        deadline: Instant,
        step: usize,
        case: usize,
        end: (u32, u32),
        extent: (u32, u32),
    }

    impl ApplicationHandler<EventPayload> for TestApp<'_> {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let monitors: Vec<_> = event_loop.available_monitors().collect();
            assert_eq!(
                monitors.len(),
                1,
                "test requires a single-output compositor"
            );
            let extent = monitors[0].size();
            self.extent = (extent.width, extent.height);
            self.app.resumed(event_loop);
        }

        fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
            self.app.new_events(event_loop, cause);
        }

        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            id: WindowId,
            event: WindowEvent,
        ) {
            self.app.window_event(event_loop, id, event);
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: EventPayload) {
            self.app.user_event(event_loop, event);
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            self.app.about_to_wait(event_loop);
            let now = Instant::now();
            assert!(now < self.deadline, "PRIMARY selection test timed out");
            if now < self.next_action {
                event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_action));
                return;
            }
            // Left drag with copy-on-select off/on, then right-button extension.
            let expected = if self.case == 2 { "RIO" } else { TEXT };
            let button = if self.case == 2 { 0x111 } else { 0x110 };
            match self.step {
                0 => {
                    let screen = &mut self
                        .app
                        .router
                        .routes
                        .values_mut()
                        .next()
                        .unwrap()
                        .window
                        .screen;
                    let (context, margin) = screen
                        .context_manager
                        .current_grid()
                        .current_context_with_computed_dimension();
                    let ready = context.terminal.lock().grid
                        [Pos::new(Line(0), Column(TEXT.len() - 1))]
                    .c() == 'N';
                    if ready {
                        let cell = context.dimension.cell;
                        let x = (margin.left + cell.cell_width as f32 * 0.2) as u32;
                        let y = (margin.top + cell.cell_height as f32 * 0.5) as u32;
                        self.end = (
                            (margin.left
                                + cell.cell_width as f32 * (expected.len() as f32 - 0.1))
                                as u32,
                            y,
                        );
                        if self.case != 2 {
                            screen.clear_selection();
                        }
                        screen.mouse.click_state = ClickState::None;
                        self.app.config.copy_on_select = self.case == 1;
                        store_external(true, "stale primary from another application");
                        store_external(false, CLIPBOARD_SENTINEL);
                        self.input.move_to(x, y, self.extent.0, self.extent.1);
                        self.step = 1;
                    }
                }
                1 => {
                    self.input.button(button, wl_pointer::ButtonState::Pressed);
                    self.step = 2;
                }
                2 => {
                    self.input.move_to(
                        self.end.0,
                        self.end.1,
                        self.extent.0,
                        self.extent.1,
                    );
                    self.step = 3;
                }
                3 => {
                    self.input.button(button, wl_pointer::ButtonState::Released);
                    self.step = 4;
                }
                4 => {
                    let screen = &self
                        .app
                        .router
                        .routes
                        .values()
                        .next()
                        .unwrap()
                        .window
                        .screen;
                    assert_eq!(
                        screen
                            .context_manager
                            .current()
                            .terminal
                            .lock()
                            .selection_to_string()
                            .as_deref(),
                        Some(expected)
                    );
                    assert_eq!(
                        load_external(true),
                        expected,
                        "PRIMARY was not claimed (copy-on-select={})",
                        self.case == 1
                    );
                    assert_eq!(
                        load_external(false),
                        if self.case == 1 {
                            TEXT
                        } else {
                            CLIPBOARD_SENTINEL
                        }
                    );
                    self.case += 1;
                    if self.case == 3 {
                        event_loop.exit();
                        return;
                    }
                    self.step = 0;
                }
                _ => unreachable!(),
            }
            self.next_action = now + Duration::from_millis(150);
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_action));
        }

        fn exiting(&mut self, _: &ActiveEventLoop) {
            // Avoid Application::exiting's process::exit, and drop the clipboard
            // worker before the Wayland display owned by the event loop.
            self.app.router.routes.clear();
            self.app.router.clipboard = Clipboard::new_nop();
            assert_eq!(self.case, 3);
        }
    }

    let input = Pointer::new();
    let mut builder = EventLoop::<EventPayload>::with_user_event();
    builder.with_wayland().with_any_thread(true);
    let mut event_loop = builder.build().unwrap();
    let mut config = rio_backend::config::Config::default();
    config.bindings.keys.clear();
    config.shell_integration = false;
    config.navigation.mode = rio_backend::config::navigation::NavigationMode::Plain;
    config.window.mode = rio_backend::config::window::WindowMode::Fullscreen;
    config.window.decorations = rio_backend::config::window::Decorations::Disabled;
    config.renderer.use_cpu = true;
    config.title.content = "Rio PRIMARY selection regression test".into();
    config.shell = rio_backend::config::Shell {
        program: Some("/bin/sh".into()),
        args: vec!["-c".into(), format!("printf '{TEXT}\\n'; exec sleep 30")],
    };
    let app = Application::new(config, None, &event_loop, None);
    let now = Instant::now();
    let mut test_app = TestApp {
        app,
        input,
        next_action: now + Duration::from_millis(150),
        deadline: now + Duration::from_secs(15),
        step: 0,
        case: 0,
        end: (0, 0),
        extent: (0, 0),
    };
    // Keep the event loop alive until after TestApp is dropped, even on panic:
    // the clipboard worker borrows its Wayland display.
    event_loop.run_app_on_demand(&mut test_app).unwrap();
}
