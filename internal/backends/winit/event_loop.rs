// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

#![warn(missing_docs)]
/*!
    This module contains the event loop implementation using winit, as well as the
    [WindowAdapter] trait used by the generated code and the run-time to change
    aspects of windows on the screen.
*/
use crate::drag_resize_window::{handle_cursor_move_for_resize, handle_resize};
use crate::winitwindowadapter::WindowVisibility;
use crate::WinitWindowEventResult;
use crate::{SharedBackendData, SlintEvent};
use corelib::graphics::euclid;
use corelib::input::{KeyEvent, KeyEventType, MouseEvent};
use corelib::items::{ColorScheme, PointerEventButton};
use corelib::lengths::LogicalPoint;
use corelib::platform::PlatformError;
use corelib::window::*;
use i_slint_core as corelib;

#[allow(unused_imports)]
use std::cell::{RefCell, RefMut};
use std::rc::Rc;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::event_loop::ControlFlow;
use winit::window::ResizeDirection;

/// This enum captures run-time specific events that can be dispatched to the event loop in
/// addition to the winit events.
pub enum CustomEvent {
    /// On wasm request_redraw doesn't wake the event loop, so we need to manually send an event
    /// so that the event loop can run
    #[cfg(target_arch = "wasm32")]
    WakeEventLoopWorkaround,
    /// Slint internal: Invoke the
    UserEvent(Box<dyn FnOnce() + Send>),
    Exit,
    #[cfg(enable_accesskit)]
    Accesskit(accesskit_winit::Event),
    #[cfg(muda)]
    Muda(muda::MenuEvent, crate::muda::MudaType),
}

impl std::fmt::Debug for CustomEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(target_arch = "wasm32")]
            Self::WakeEventLoopWorkaround => write!(f, "WakeEventLoopWorkaround"),
            Self::UserEvent(_) => write!(f, "UserEvent"),
            Self::Exit => write!(f, "Exit"),
            #[cfg(enable_accesskit)]
            Self::Accesskit(a) => write!(f, "AccessKit({a:?})"),
            #[cfg(muda)]
            Self::Muda(e, mt) => write!(f, "Muda({e:?},{mt:?})"),
        }
    }
}

pub struct EventLoopState {
    shared_backend_data: Rc<SharedBackendData>,
    // last seen cursor position
    cursor_pos: LogicalPoint,
    pressed: bool,
    current_touch_id: Option<u64>,

    loop_error: Option<PlatformError>,
    current_resize_direction: Option<ResizeDirection>,

    /// Set to true when pumping events for the shortest amount of time possible.
    pumping_events_instantly: bool,

    custom_application_handler: Option<Box<dyn crate::CustomApplicationHandler>>,
}

impl EventLoopState {
    pub fn new(
        shared_backend_data: Rc<SharedBackendData>,
        custom_application_handler: Option<Box<dyn crate::CustomApplicationHandler>>,
    ) -> Self {
        Self {
            shared_backend_data,
            cursor_pos: Default::default(),
            pressed: Default::default(),
            current_touch_id: Default::default(),
            loop_error: Default::default(),
            current_resize_direction: Default::default(),
            pumping_events_instantly: Default::default(),
            custom_application_handler,
        }
    }

    /// Free graphics resources for any hidden windows. Called when quitting the event loop, to work
    /// around #8795.
    fn suspend_all_hidden_windows(&self) {
        let windows_to_suspend = self
            .shared_backend_data
            .active_windows
            .borrow()
            .values()
            .filter_map(|w| w.upgrade())
            .filter(|w| matches!(w.visibility(), WindowVisibility::Hidden))
            .collect::<Vec<_>>();
        for window in windows_to_suspend.into_iter() {
            let _ = window.suspend();
        }
    }
}

impl winit::application::ApplicationHandler<SlintEvent> for EventLoopState {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if matches!(
            self.custom_application_handler
                .as_mut()
                .map_or(WinitWindowEventResult::Propagate, |handler| {
                    handler.resumed(event_loop)
                }),
            WinitWindowEventResult::PreventDefault
        ) {
            return;
        }
        if let Err(err) = self.shared_backend_data.create_inactive_windows(event_loop) {
            self.loop_error = Some(err);
            event_loop.exit();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.shared_backend_data.window_by_id(window_id) else {
            if let Some(handler) = self.custom_application_handler.as_mut() {
                handler.window_event(event_loop, window_id, None, None, &event);
            }
            return;
        };

        if let Some(winit_window) = window.winit_window() {
            if matches!(
                self.custom_application_handler.as_mut().map_or(
                    WinitWindowEventResult::Propagate,
                    |handler| handler.window_event(
                        event_loop,
                        window_id,
                        Some(&*winit_window),
                        Some(window.window()),
                        &event
                    )
                ),
                WinitWindowEventResult::PreventDefault
            ) {
                return;
            }

            if let Some(mut window_event_filter) = window.window_event_filter.take() {
                let event_result = window_event_filter(window.window(), &event);
                window.window_event_filter.set(Some(window_event_filter));

                match event_result {
                    WinitWindowEventResult::PreventDefault => return,
                    WinitWindowEventResult::Propagate => (),
                }
            }

            #[cfg(enable_accesskit)]
            window
                .accesskit_adapter()
                .expect("internal error: accesskit adapter must exist when window exists")
                .borrow_mut()
                .process_event(&winit_window, &event);
        } else {
            return;
        }

        let runtime_window = WindowInner::from_pub(window.window());
        match event {
            WindowEvent::RedrawRequested => {
                self.loop_error = window.draw().err();
            }
            WindowEvent::Resized(size) => {
                self.loop_error = window.resize_event(size).err();

                // Entering fullscreen, maximizing or minimizing the window will
                // trigger a resize event. We need to update the internal window
                // state to match the actual window state. We simulate a "window
                // state event" since there is not an official event for it yet.
                // Because we don't always get a Resized event (eg, minimized), also handle Occluded
                // See: https://github.com/rust-windowing/winit/issues/2334
                window.window_state_event();
            }
            WindowEvent::CloseRequested => {
                self.loop_error = window
                    .window()
                    .try_dispatch_event(corelib::platform::WindowEvent::CloseRequested)
                    .err();
            }
            WindowEvent::Focused(have_focus) => {
                self.loop_error = window.activation_changed(have_focus).err();
            }

            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                let key_code = event.logical_key;
                // For now: Match Qt's behavior of mapping command to control and control to meta (LWin/RWin).
                cfg_if::cfg_if!(
                    if #[cfg(target_vendor = "apple")] {
                        let swap_cmd_ctrl = true;
                    } else if #[cfg(target_family = "wasm")] {
                        let swap_cmd_ctrl = web_sys::window()
                            .and_then(|window| window.navigator().platform().ok())
                            .is_some_and(|platform| {
                                let platform = platform.to_ascii_lowercase();
                                platform.contains("mac")
                                    || platform.contains("iphone")
                                    || platform.contains("ipad")
                            });
                    } else {
                        let swap_cmd_ctrl = false;
                    }
                );

                let key_code = if swap_cmd_ctrl {
                    match key_code {
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Control) => {
                            winit::keyboard::Key::Named(winit::keyboard::NamedKey::Super)
                        }
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::Super) => {
                            winit::keyboard::Key::Named(winit::keyboard::NamedKey::Control)
                        }
                        code => code,
                    }
                } else {
                    key_code
                };

                macro_rules! winit_key_to_char {
                ($($char:literal # $name:ident # $($_qt:ident)|* # $($winit:ident $(($pos:ident))?)|* # $($_xkb:ident)|*;)*) => {
                    match &key_code {
                        $($(winit::keyboard::Key::Named(winit::keyboard::NamedKey::$winit) $(if event.location == winit::keyboard::KeyLocation::$pos)? => $char.into(),)*)*
                        winit::keyboard::Key::Character(str) => str.as_str().into(),
                        _ => {
                            if let Some(text) = &event.text {
                                text.as_str().into()
                            } else {
                                return;
                            }
                        }
                    }
                }
            }
                let text = i_slint_common::for_each_special_keys!(winit_key_to_char);

                self.loop_error = window
                    .window()
                    .try_dispatch_event(match event.state {
                        winit::event::ElementState::Pressed if event.repeat => {
                            corelib::platform::WindowEvent::KeyPressRepeated { text }
                        }
                        winit::event::ElementState::Pressed => {
                            if is_synthetic {
                                // Synthetic event are sent when the focus is acquired, for all the keys currently pressed.
                                // Don't forward these keys other than modifiers to the app
                                use winit::keyboard::{Key::Named, NamedKey as N};
                                if !matches!(
                                    key_code,
                                    Named(N::Control | N::Shift | N::Super | N::Alt | N::AltGraph),
                                ) {
                                    return;
                                }
                            }
                            corelib::platform::WindowEvent::KeyPressed { text }
                        }
                        winit::event::ElementState::Released => {
                            corelib::platform::WindowEvent::KeyReleased { text }
                        }
                    })
                    .err();
            }
            WindowEvent::Ime(winit::event::Ime::Preedit(string, preedit_selection)) => {
                let event = KeyEvent {
                    event_type: KeyEventType::UpdateComposition,
                    preedit_text: string.into(),
                    preedit_selection: preedit_selection.map(|e| e.0 as i32..e.1 as i32),
                    ..Default::default()
                };
                runtime_window.process_key_input(event);
            }
            WindowEvent::Ime(winit::event::Ime::Commit(string)) => {
                let event = KeyEvent {
                    event_type: KeyEventType::CommitComposition,
                    text: string.into(),
                    ..Default::default()
                };
                runtime_window.process_key_input(event);
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.current_resize_direction = handle_cursor_move_for_resize(
                    &window.winit_window().unwrap(),
                    position,
                    self.current_resize_direction,
                    runtime_window
                        .window_item()
                        .map_or(0_f64, |w| w.as_pin_ref().resize_border_width().get().into()),
                );
                let position = position.to_logical(runtime_window.scale_factor() as f64);
                self.cursor_pos = euclid::point2(position.x, position.y);
                runtime_window.process_mouse_input(MouseEvent::Moved { position: self.cursor_pos });
            }
            WindowEvent::CursorLeft { .. } => {
                // On the html canvas, we don't get the mouse move or release event when outside the canvas. So we have no choice but canceling the event
                if cfg!(target_arch = "wasm32") || !self.pressed {
                    self.pressed = false;
                    runtime_window.process_mouse_input(MouseEvent::Exit);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (delta_x, delta_y) = match delta {
                    winit::event::MouseScrollDelta::LineDelta(lx, ly) => (lx * 60., ly * 60.),
                    winit::event::MouseScrollDelta::PixelDelta(d) => {
                        let d = d.to_logical(runtime_window.scale_factor() as f64);
                        (d.x, d.y)
                    }
                };
                runtime_window.process_mouse_input(MouseEvent::Wheel {
                    position: self.cursor_pos,
                    delta_x,
                    delta_y,
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    winit::event::MouseButton::Left => PointerEventButton::Left,
                    winit::event::MouseButton::Right => PointerEventButton::Right,
                    winit::event::MouseButton::Middle => PointerEventButton::Middle,
                    winit::event::MouseButton::Back => PointerEventButton::Back,
                    winit::event::MouseButton::Forward => PointerEventButton::Forward,
                    winit::event::MouseButton::Other(_) => PointerEventButton::Other,
                };
                let ev = match state {
                    winit::event::ElementState::Pressed => {
                        if button == PointerEventButton::Left
                            && self.current_resize_direction.is_some()
                        {
                            handle_resize(
                                &window.winit_window().unwrap(),
                                self.current_resize_direction,
                            );
                            return;
                        }

                        self.pressed = true;
                        MouseEvent::Pressed { position: self.cursor_pos, button, click_count: 0 }
                    }
                    winit::event::ElementState::Released => {
                        self.pressed = false;
                        MouseEvent::Released { position: self.cursor_pos, button, click_count: 0 }
                    }
                };
                runtime_window.process_mouse_input(ev);
            }
            WindowEvent::Touch(touch) => {
                if Some(touch.id) == self.current_touch_id || self.current_touch_id.is_none() {
                    let location = touch.location.to_logical(runtime_window.scale_factor() as f64);
                    let position = euclid::point2(location.x, location.y);
                    let ev = match touch.phase {
                        winit::event::TouchPhase::Started => {
                            self.pressed = true;
                            if self.current_touch_id.is_none() {
                                self.current_touch_id = Some(touch.id);
                            }
                            MouseEvent::Pressed {
                                position,
                                button: PointerEventButton::Left,
                                click_count: 0,
                            }
                        }
                        winit::event::TouchPhase::Ended | winit::event::TouchPhase::Cancelled => {
                            self.pressed = false;
                            self.current_touch_id = None;
                            MouseEvent::Released {
                                position,
                                button: PointerEventButton::Left,
                                click_count: 0,
                            }
                        }
                        winit::event::TouchPhase::Moved => MouseEvent::Moved { position },
                    };
                    runtime_window.process_mouse_input(ev);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, inner_size_writer: _ } => {
                if std::env::var("SLINT_SCALE_FACTOR").is_err() {
                    self.loop_error = window
                        .window()
                        .try_dispatch_event(corelib::platform::WindowEvent::ScaleFactorChanged {
                            scale_factor: scale_factor as f32,
                        })
                        .err();
                    // TODO: send a resize event or try to keep the logical size the same.
                    //window.resize_event(inner_size_writer.???)?;
                }
            }
            WindowEvent::ThemeChanged(theme) => window.set_color_scheme(match theme {
                winit::window::Theme::Dark => ColorScheme::Dark,
                winit::window::Theme::Light => ColorScheme::Light,
            }),
            WindowEvent::Occluded(x) => {
                window.renderer.occluded(x);

                // In addition to the hack done for WindowEvent::Resize, also do it for Occluded so we handle Minimized change
                window.window_state_event();
            }
            _ => {}
        }

        if self.loop_error.is_some() {
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: SlintEvent) {
        match event.0 {
            CustomEvent::UserEvent(user_callback) => user_callback(),
            CustomEvent::Exit => {
                self.suspend_all_hidden_windows();
                event_loop.exit()
            }
            #[cfg(enable_accesskit)]
            CustomEvent::Accesskit(accesskit_winit::Event { window_id, window_event }) => {
                if let Some(window) = self.shared_backend_data.window_by_id(window_id) {
                    let deferred_action = window
                        .accesskit_adapter()
                        .expect("internal error: accesskit adapter must exist when window exists")
                        .borrow_mut()
                        .process_accesskit_event(window_event);
                    // access kit adapter not borrowed anymore, now invoke the deferred action
                    if let Some(deferred_action) = deferred_action {
                        deferred_action.invoke(window.window());
                    }
                }
            }
            #[cfg(target_arch = "wasm32")]
            CustomEvent::WakeEventLoopWorkaround => {
                event_loop.set_control_flow(ControlFlow::Poll);
            }
            #[cfg(muda)]
            CustomEvent::Muda(event, muda_type) => {
                if let Some((window, eid)) = event.id().0.split_once('|').and_then(|(w, e)| {
                    Some((
                        self.shared_backend_data
                            .window_by_id(winit::window::WindowId::from(w.parse::<u64>().ok()?))?,
                        e.parse::<usize>().ok()?,
                    ))
                }) {
                    window.muda_event(eid, muda_type);
                };
            }
        }
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if matches!(
            self.custom_application_handler
                .as_mut()
                .map_or(WinitWindowEventResult::Propagate, |handler| {
                    handler.new_events(event_loop, cause)
                }),
            WinitWindowEventResult::PreventDefault
        ) {
            return;
        }

        event_loop.set_control_flow(ControlFlow::Wait);

        corelib::platform::update_timers_and_animations();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if matches!(
            self.custom_application_handler
                .as_mut()
                .map_or(WinitWindowEventResult::Propagate, |handler| {
                    handler.about_to_wait(event_loop)
                }),
            WinitWindowEventResult::PreventDefault
        ) {
            return;
        }

        if let Err(err) = self.shared_backend_data.create_inactive_windows(event_loop) {
            self.loop_error = Some(err);
        }

        if !event_loop.exiting() {
            for w in self
                .shared_backend_data
                .active_windows
                .borrow()
                .iter()
                .filter_map(|(_, w)| w.upgrade())
            {
                if w.window().has_active_animations() {
                    w.request_redraw();
                }
            }
        }

        if event_loop.control_flow() == ControlFlow::Wait {
            if let Some(next_timer) = corelib::platform::duration_until_next_timer_update() {
                event_loop.set_control_flow(ControlFlow::wait_duration(next_timer));
            }
        }

        if self.pumping_events_instantly {
            event_loop.set_control_flow(ControlFlow::Poll);
        }
    }

    fn device_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        device_id: winit::event::DeviceId,
        event: winit::event::DeviceEvent,
    ) {
        if let Some(handler) = self.custom_application_handler.as_mut() {
            handler.device_event(event_loop, device_id, event);
        }
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(handler) = self.custom_application_handler.as_mut() {
            handler.suspended(event_loop);
        }
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(handler) = self.custom_application_handler.as_mut() {
            handler.exiting(event_loop);
        }
    }

    fn memory_warning(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(handler) = self.custom_application_handler.as_mut() {
            handler.memory_warning(event_loop);
        }
    }
}

impl EventLoopState {
    /// Runs the event loop and renders the items in the provided `component` in its
    /// own window.
    #[allow(unused_mut)] // mut need changes for wasm
    pub fn run(mut self) -> Result<Self, corelib::platform::PlatformError> {
        let not_running_loop_instance = self
            .shared_backend_data
            .not_running_event_loop
            .take()
            .ok_or_else(|| PlatformError::from("Nested event loops are not supported"))?;
        let mut winit_loop = not_running_loop_instance;

        cfg_if::cfg_if! {
            if #[cfg(any(target_arch = "wasm32", ios_and_friends))] {
                winit_loop
                    .run_app(&mut self)
                    .map_err(|e| format!("Error running winit event loop: {e}"))?;
                // This can't really happen, as run() doesn't return
                Ok(Self::new(self.shared_backend_data.clone(), None))
            } else {
                use winit::platform::run_on_demand::EventLoopExtRunOnDemand as _;
                winit_loop
                    .run_app_on_demand(&mut self)
                    .map_err(|e| format!("Error running winit event loop: {e}"))?;

                // Keep the EventLoop instance alive and re-use it in future invocations of run_event_loop().
                // Winit does not support creating multiple instances of the event loop.
                self.shared_backend_data.not_running_event_loop.replace(Some(winit_loop));

                if let Some(error) = self.loop_error {
                    return Err(error);
                }
                Ok(self)
            }
        }
    }

    /// Runs the event loop and renders the items in the provided `component` in its
    /// own window.
    #[cfg(all(not(target_arch = "wasm32"), not(ios_and_friends)))]
    pub fn pump_events(
        mut self,
        timeout: Option<std::time::Duration>,
    ) -> Result<(Self, winit::platform::pump_events::PumpStatus), corelib::platform::PlatformError>
    {
        use winit::platform::pump_events::EventLoopExtPumpEvents;

        let not_running_loop_instance = self
            .shared_backend_data
            .not_running_event_loop
            .take()
            .ok_or_else(|| PlatformError::from("Nested event loops are not supported"))?;
        let mut winit_loop = not_running_loop_instance;

        self.pumping_events_instantly = timeout.is_some_and(|duration| duration.is_zero());

        let result = winit_loop.pump_app_events(timeout, &mut self);

        self.pumping_events_instantly = false;

        // Keep the EventLoop instance alive and re-use it in future invocations of run_event_loop().
        // Winit does not support creating multiple instances of the event loop.
        self.shared_backend_data.not_running_event_loop.replace(Some(winit_loop));

        if let Some(error) = self.loop_error {
            return Err(error);
        }
        Ok((self, result))
    }

    #[cfg(target_arch = "wasm32")]
    pub fn spawn(self) -> Result<(), corelib::platform::PlatformError> {
        use winit::platform::web::EventLoopExtWebSys;
        let not_running_loop_instance = self
            .shared_backend_data
            .not_running_event_loop
            .take()
            .ok_or_else(|| PlatformError::from("Nested event loops are not supported"))?;

        not_running_loop_instance.spawn_app(self);

        Ok(())
    }
}
