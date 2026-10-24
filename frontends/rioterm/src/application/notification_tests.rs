use super::*;
use rio_window::platform::{
    run_on_demand::EventLoopExtRunOnDemand,
    wayland::{EventLoopBuilderExtWayland, WindowExtWayland},
};

/// Uses real windows/rendering to verify tab/split identity, reordering, stale
/// clicks and quake visibility. Requires a compositor with a keyboard seat.
/// Point RIO_CONFIG_HOME at a disposable directory when running this test.
#[test]
#[ignore = "requires a running Wayland compositor with keyboard focus"]
fn notification_click_focuses_originating_terminal() {
    struct TestApp<'a> {
        app: Application<'a>,
        quake: Option<rio_backend::event::WindowId>,
        normal_windows: usize,
        phase: usize,
        next_action: Instant,
        deadline: Instant,
    }

    impl TestApp<'_> {
        fn click(
            &mut self,
            event_loop: &ActiveEventLoop,
            id: rio_backend::event::WindowId,
        ) {
            let route_id = self
                .app
                .router
                .routes
                .get(&id)
                .map(|route| route.window.screen.ctx().current().route_id)
                .unwrap_or(usize::MAX);
            self.click_pane(event_loop, id, route_id);
        }

        fn click_pane(
            &mut self,
            event_loop: &ActiveEventLoop,
            id: rio_backend::event::WindowId,
            route_id: usize,
        ) {
            self.app.user_event(
                event_loop,
                EventPayload::new(
                    RioEventType::Rio(RioEvent::FocusWindowFromNotification {
                        route_id,
                        activation_token: None,
                    }),
                    id,
                ),
            );
        }

        fn check_tab_and_split_focus(
            &mut self,
            event_loop: &ActiveEventLoop,
            id: rio_backend::event::WindowId,
        ) {
            let (first, target, away) = {
                let screen =
                    &mut self.app.router.routes.get_mut(&id).unwrap().window.screen;
                let clipboard = &mut self.app.router.clipboard;
                let first = screen.ctx().current().route_id;
                screen.split_right();
                let target = screen.ctx().current_route();
                screen.split_down(); // Leave the notifying split inactive.
                screen.create_tab(clipboard);
                screen.create_tab(clipboard);
                let away = screen.ctx().current_route();
                screen.ctx_mut().contexts_mut().swap(0, 1);
                (first, target, away)
            };
            self.click_pane(event_loop, id, target);
            let screen = &self.app.router.routes[&id].window.screen;
            assert_eq!(screen.ctx().current_index(), 1);
            assert_eq!(screen.ctx().current_grid_len(), 3);
            assert_eq!(screen.ctx().current_route(), target);
            assert_eq!(screen.ctx().current().route_id, target);
            assert!(screen
                .ctx()
                .current()
                .renderable_content
                .pending_update
                .is_dirty());

            // Another split of the already-active tab, then the notifying split.
            self.click_pane(event_loop, id, first);
            assert_eq!(
                self.app.router.routes[&id]
                    .window
                    .screen
                    .ctx()
                    .current_route(),
                first
            );
            self.click_pane(event_loop, id, target);
            assert_eq!(
                self.app.router.routes[&id]
                    .window
                    .screen
                    .ctx()
                    .current_route(),
                target
            );

            self.app
                .router
                .routes
                .get_mut(&id)
                .unwrap()
                .window
                .screen
                .ctx_mut()
                .move_current_tab_to(2);
            self.click_pane(event_loop, id, away);
            self.click_pane(event_loop, id, target);
            assert_eq!(
                self.app.router.routes[&id]
                    .window
                    .screen
                    .ctx()
                    .current_index(),
                2
            );
            assert_eq!(
                self.app.router.routes[&id]
                    .window
                    .screen
                    .ctx()
                    .current_route(),
                target
            );

            self.app
                .router
                .routes
                .get_mut(&id)
                .unwrap()
                .window
                .screen
                .close_split_or_tab(&mut self.app.router.clipboard);
            self.click_pane(event_loop, id, away);
            self.app.router.routes[&id]
                .window
                .winit_window
                .set_visible(false);
            self.click_pane(event_loop, id, target); // Its split no longer exists.
            assert_eq!(
                self.app.router.routes[&id]
                    .window
                    .screen
                    .ctx()
                    .current_route(),
                away
            );
            assert_eq!(
                self.app.router.routes[&id].window.winit_window.is_visible(),
                Some(false)
            );
            self.click_pane(event_loop, id, first);
            assert_eq!(
                self.app.router.routes[&id]
                    .window
                    .screen
                    .ctx()
                    .current_route(),
                first
            );
        }
    }

    impl ApplicationHandler<EventPayload> for TestApp<'_> {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
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
            assert!(
                now < self.deadline,
                "notification click did not focus quake"
            );
            if now < self.next_action {
                event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_action));
                return;
            }
            match self.phase {
                0 => {
                    self.normal_windows = self.app.router.routes.len();
                    assert_eq!(self.normal_windows, 1);
                    let normal = *self.app.router.routes.keys().next().unwrap();
                    self.check_tab_and_split_focus(event_loop, normal);
                    self.click(event_loop, normal);
                    assert_eq!(
                        self.app.router.routes[&normal]
                            .window
                            .winit_window
                            .is_visible(),
                        Some(true)
                    );
                    self.app.router.create_quake_window(
                        event_loop,
                        self.app.event_proxy.clone(),
                        &self.app.config,
                        event_loop.available_monitors().next(),
                    );
                    let id = self.app.router.quake_window_id.unwrap();
                    let window = &self.app.router.routes[&id].window.winit_window;
                    assert!(window.is_quake_overlay());
                    assert_eq!(window.is_visible(), Some(false));
                    self.quake = Some(id);
                    self.phase = 1;
                }
                1 | 3 => {
                    self.click(event_loop, self.quake.unwrap());
                    assert_eq!(
                        self.app.router.routes[&self.quake.unwrap()]
                            .window
                            .winit_window
                            .is_visible(),
                        Some(true)
                    );
                    assert_eq!(self.app.router.routes.len(), self.normal_windows + 1);
                    self.phase += 1;
                }
                2 | 4 => {
                    let id = self.quake.unwrap();
                    let window = &self.app.router.routes[&id].window.winit_window;
                    if window.has_focus() {
                        if self.phase == 2 {
                            self.check_tab_and_split_focus(event_loop, id);
                            // Clicking again while visible must not toggle it off.
                            self.click(event_loop, id);
                            let window = &self.app.router.routes[&id].window.winit_window;
                            assert_eq!(window.is_visible(), Some(true));
                            window.set_visible(false);
                            self.phase = 3;
                        } else {
                            let normal = *self
                                .app
                                .router
                                .routes
                                .keys()
                                .find(|key| **key != id)
                                .unwrap();
                            self.click(event_loop, normal);
                            assert_eq!(
                                self.app.router.routes[&id]
                                    .window
                                    .winit_window
                                    .is_visible(),
                                Some(false)
                            );
                            self.app.router.routes.remove(&id);
                            self.click(event_loop, id);
                            assert_eq!(self.app.router.routes.len(), self.normal_windows);
                            self.phase = 5;
                            event_loop.exit();
                        }
                    }
                }
                _ => unreachable!(),
            }
            self.next_action = now + Duration::from_millis(100);
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_action));
        }

        fn exiting(&mut self, _: &ActiveEventLoop) {
            // Application::exiting calls process::exit, which would hide test
            // results. Do its resource cleanup without terminating libtest.
            self.app.router.routes.clear();
            self.app.router.clipboard = Clipboard::new_nop();
            assert_eq!(self.phase, 5, "application exited before all click checks");
        }
    }

    let mut builder = EventLoop::<EventPayload>::with_user_event();
    builder.with_wayland().with_any_thread(true);
    let mut event_loop = builder.build().unwrap();
    let mut config = rio_backend::config::Config::default();
    config.bindings.keys.clear();
    config.shell_integration = false;
    config.renderer.use_cpu = true;
    config.navigation.mode = rio_backend::config::navigation::NavigationMode::Plain;
    config.title.content = "Rio notification focus regression test".into();
    config.window.width = 480;
    config.window.height = 320;
    config.window.quake_width_percentage = 0.25;
    config.window.quake_height_percentage = 0.25;
    config.shell = rio_backend::config::Shell {
        program: Some("sh".into()),
        args: vec!["-c".into(), "sleep 30".into()],
    };
    let app = Application::new(config, None, &event_loop, None);
    let now = Instant::now();
    let mut test_app = TestApp {
        app,
        quake: None,
        normal_windows: 0,
        phase: 0,
        next_action: now + Duration::from_millis(100),
        deadline: now + Duration::from_secs(15),
    };
    event_loop.run_app_on_demand(&mut test_app).unwrap();
}
