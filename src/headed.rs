use ruffle_core::backend::audio::NullAudioBackend;
use ruffle_core::events::MouseButton as RuffleMouseButton;
use ruffle_core::tag_utils::SwfMovie;
use ruffle_core::{FloatDuration, Player, PlayerBuilder, PlayerEvent};
use ruffle_render::backend::ViewportDimensions;
use ruffle_render::quality::StageQuality;
use ruffle_render_wgpu::backend::WgpuRenderBackend;
use ruffle_render_wgpu::wgpu;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::authenticator::PreparedLaunch;
use crate::bridge::Bridge;
use crate::desktop_ui::DesktopUi;
use crate::executor::GameExecutor;
use crate::keyboard::{winit_input_to_ruffle_key_descriptor, winit_to_ruffle_text_control};
use crate::navigator::GameNavigator;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Modifiers, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

pub async fn run_headed(
    launch: PreparedLaunch,
    script_timeout: u64,
    quality: StageQuality,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("[Mode] Running headed");

    let event_loop = EventLoop::new()?;
    let watchdog = UiWatchdog::new();
    let mut app = HeadedApp {
        movie: Some(launch.movie),
        network: Some(launch.network),
        window_size: (launch.width, launch.height),
        script_timeout,
        quality,
        smoke_pool: launch.smoke_pool,
        bridge: launch.bridge,
        executor: None,
        window: None,
        player: None,
        mouse_pos: PhysicalPosition::new(0.0, 0.0),
        modifiers: Modifiers::default(),
        last_tick: Instant::now(),
        error: None,
        watchdog,
    };
    event_loop.run_app(&mut app)?;

    if let Some(error) = app.error.take() {
        return Err(error);
    }
    Ok(())
}

struct HeadedApp {
    movie: Option<SwfMovie>,
    network: Option<crate::navigator::GameNetwork>,
    window_size: (u32, u32),
    /// Polled each frame so preloader fetches spawned on this thread can finish.
    executor: Option<GameExecutor>,
    /// Kept alive for the wgpu surface created from this window.
    window: Option<Arc<Window>>,
    player: Option<Arc<Mutex<Player>>>,
    mouse_pos: PhysicalPosition<f64>,
    modifiers: Modifiers,
    last_tick: Instant,
    error: Option<Box<dyn std::error::Error>>,
    watchdog: UiWatchdog,
    script_timeout: u64,
    quality: StageQuality,
    smoke_pool: Option<u32>,
    bridge: Bridge,
}

impl HeadedApp {
    fn start(
        &mut self,
        event_loop: &ActiveEventLoop,
        movie: SwfMovie,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (stage_width, stage_height) = self.window_size;
        let window = Arc::new(
            event_loop.create_window(
                Window::default_attributes()
                    .with_title("Xenomit")
                    .with_inner_size(LogicalSize::new(stage_width as f64, stage_height as f64)),
            )?,
        );
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);
        let scale_factor = window.scale_factor();

        // The window outlives the swapchain: `self.window` holds it until the
        // player (and its surface) is dropped first.
        let target = unsafe { wgpu::SurfaceTargetUnsafe::from_window(window.as_ref())? };
        let renderer = unsafe {
            WgpuRenderBackend::for_window_unsafe(
                target,
                (width, height),
                wgpu::Backends::PRIMARY,
                wgpu::PowerPreference::HighPerformance,
                Some(Box::new(Arc::clone(&window))),
            )
        }?;

        let network = self.network.take().ok_or("game session was not loaded")?;
        let page_url = network.page_url.clone();
        let executor = GameExecutor::new();
        let navigator = GameNavigator::new(&executor, &network, self.smoke_pool)?;
        let player = PlayerBuilder::new()
            .with_renderer(renderer)
            .with_audio(NullAudioBackend::new())
            .with_ui(DesktopUi::new())
            .with_navigator(navigator)
            .with_external_interface(Box::new(self.bridge.provider(Some(page_url.clone()))))
            .with_page_url(Some(page_url))
            .with_viewport_dimensions(width, height, scale_factor)
            .with_autoplay(true)
            .with_max_execution_duration(Duration::from_secs(self.script_timeout))
            .with_quality(self.quality)
            .with_movie(movie)
            .build();
        self.executor = Some(executor);

        self.bridge.spawn_pipe();
        window.request_redraw();
        self.window = Some(window);
        self.player = Some(player);
        self.last_tick = Instant::now();
        Ok(())
    }

    fn viewport(&self, width: u32, height: u32) -> ViewportDimensions {
        ViewportDimensions {
            width,
            height,
            scale_factor: self
                .window
                .as_ref()
                .map(|window| window.scale_factor())
                .unwrap_or(1.0),
        }
    }

    fn with_player(&self, operation: &'static str, f: impl FnOnce(&mut Player)) {
        if let Some(player) = &self.player {
            let _guard = self.watchdog.enter(operation);
            f(&mut player.lock().expect("player lock"));
        }
    }
}

impl ApplicationHandler for HeadedApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.player.is_some() {
            return;
        }
        let Some(movie) = self.movie.take() else {
            event_loop.exit();
            return;
        };
        if let Err(error) = self.start(event_loop, movie) {
            self.error = Some(error);
            event_loop.exit();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                self.with_player("render", |player| player.render());
            }
            WindowEvent::Resized(size) => {
                if size.width == 0 || size.height == 0 {
                    return;
                }
                let viewport = self.viewport(size.width, size.height);
                self.with_player("resize", |player| player.set_viewport_dimensions(viewport));
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = position;
                self.with_player("mouse move", |player| {
                    player.handle_event(PlayerEvent::MouseMove {
                        x: position.x,
                        y: position.y,
                    });
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = ruffle_mouse_button(button);
                let x = self.mouse_pos.x;
                let y = self.mouse_pos.y;
                self.with_player("mouse click", |player| {
                    let event = match state {
                        ElementState::Pressed => PlayerEvent::MouseDown {
                            x,
                            y,
                            button,
                            index: None,
                        },
                        ElementState::Released => PlayerEvent::MouseUp { x, y, button },
                    };
                    player.handle_event(event);
                });
            }
            WindowEvent::CursorLeft { .. } => {
                self.with_player("mouse leave", |player| {
                    player.handle_event(PlayerEvent::MouseLeave);
                });
            }
            WindowEvent::Focused(focused) => {
                self.with_player("focus", |player| {
                    player.handle_event(if focused {
                        PlayerEvent::FocusGained
                    } else {
                        PlayerEvent::FocusLost
                    });
                });
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers,
            WindowEvent::KeyboardInput { event, .. } => {
                let key = winit_input_to_ruffle_key_descriptor(&event);
                let control = winit_to_ruffle_text_control(&event, self.modifiers);
                self.with_player("keyboard", |player| match event.state {
                    ElementState::Pressed => {
                        player.handle_event(PlayerEvent::KeyDown { key });
                        if let Some(code) = control {
                            player.handle_event(PlayerEvent::TextControl { code });
                        } else if let Some(text) = event.text {
                            for codepoint in text.chars() {
                                player.handle_event(PlayerEvent::TextInput { codepoint });
                            }
                        }
                    }
                    ElementState::Released => {
                        player.handle_event(PlayerEvent::KeyUp { key });
                    }
                });
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(player) = &self.player else {
            return;
        };
        let now = Instant::now();
        let dt = FloatDuration::from_std(now.duration_since(self.last_tick));
        if dt.as_millis() <= 0.0 {
            return;
        }
        self.last_tick = now;

        if let Some(executor) = &mut self.executor {
            let _guard = self.watchdog.enter("network poll");
            executor.run();
        }
        let (frame_delay, needs_render) = {
            let _guard = self.watchdog.enter("Flash tick");
            let mut player = player.lock().expect("player lock");
            player.tick(dt);
            self.bridge.poll(&mut player);
            (player.time_til_next_frame(), player.needs_render())
        };
        if let Some(executor) = &mut self.executor {
            let _guard = self.watchdog.enter("network poll");
            executor.run();
        }
        if self.bridge.close_requested() {
            event_loop.exit();
            return;
        }
        if needs_render {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        let delay = frame_delay.clamp(Duration::from_millis(1), Duration::from_millis(16));
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + delay));
    }
}

struct UiWatchdog(Arc<Mutex<(&'static str, Instant)>>);

impl UiWatchdog {
    fn new() -> Self {
        let state = Arc::new(Mutex::new(("idle", Instant::now())));
        let weak = Arc::downgrade(&state);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let Some(state) = weak.upgrade() else { break };
                let (operation, since) = *state.lock().expect("watchdog state");
                if operation != "idle" && since.elapsed() >= Duration::from_secs(2) {
                    eprintln!(
                        "[watchdog] window thread is in {operation} for {}s",
                        since.elapsed().as_secs()
                    );
                }
            }
        });
        Self(state)
    }

    fn enter(&self, operation: &'static str) -> UiOperation<'_> {
        *self.0.lock().expect("watchdog state") = (operation, Instant::now());
        UiOperation(self)
    }
}

struct UiOperation<'a>(&'a UiWatchdog);

impl Drop for UiOperation<'_> {
    fn drop(&mut self) {
        *self.0.0.lock().expect("watchdog state") = ("idle", Instant::now());
    }
}

fn ruffle_mouse_button(button: MouseButton) -> RuffleMouseButton {
    match button {
        MouseButton::Left => RuffleMouseButton::Left,
        MouseButton::Right => RuffleMouseButton::Right,
        MouseButton::Middle => RuffleMouseButton::Middle,
        _ => RuffleMouseButton::Unknown,
    }
}
