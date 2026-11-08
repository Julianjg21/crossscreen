//! Modo `--listen`: hilo de la ventana (winit).
//!
//! - Arranca el hilo de red con el `native_mode` y los dpi del monitor elegido.
//! - Por cada sesión crea la ventana, el `Renderer` y el hilo de
//!   decodificación (mismo dispositivo D3D11) y responde «listo» a la red,
//!   que entonces envía `StreamStarted`.
//! - Presenta: espera al objeto *waitable* de la cadena (como mucho un
//!   *vblank*), recoge el frame más reciente del buzón y dibuja vídeo +
//!   cursor. Los mensajes de cursor repintan enseguida con el último frame,
//!   sin esperar a un frame de vídeo nuevo.

use crate::decode_worker::{self, Worker};
use crate::net::{self, NetConfig};
use crate::session::{
    LossPolicy, MailFrame, Mailbox, SessionStats, StartSession, UiCmd, UserEvent, FREEZE_THRESHOLD,
};
use crate::{clock, parse_size, save_png, Args};
use anyhow::{anyhow, Context, Result};
use cs_core::Mode;
use cs_decode::create_d3d11_device;
use cs_protocol::control::{ButtonState, InputEvent, MouseButton as ProtoMouseButton};
use cs_render::{FrameRef, Renderer, RendererOptions};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton as WinitMouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::monitor::MonitorHandle;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Fullscreen, Window, WindowId};

/// Tiempo máximo de espera al objeto *waitable* antes de dibujar.
const WAITABLE_TIMEOUT: Duration = Duration::from_millis(50);
/// Margen antes del *vblank* para un repintado solo de cursor: dibujar
/// (~1–2 ms en la Iris Xe) y `Present` deben caber antes del *vblank*.
const CURSOR_LATCH_MARGIN: Duration = Duration::from_millis(4);
/// Si la red no termina tras pedir el cierre, salir igualmente.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

pub fn main(args: Args) -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let device = create_d3d11_device().context("creando el dispositivo D3D11")?;
    let mut app = ListenApp {
        args,
        proxy,
        device,
        net: None,
        net_cmd: None,
        monitor: None,
        session: None,
        deadline: None,
        shutdown_at: None,
        refresh: Duration::from_micros(16_667),
        error: None,
    };
    event_loop.run_app(&mut app)?;
    app.end_session("el visor se cierra");
    app.net_cmd = None;
    if let Some(h) = app.net.take() {
        let _ = h.join();
    }
    match app.error.take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Una sesión en curso. Orden de destrucción: frame actual → buzón → hilo de
/// decodificación (suelta el MFT) → renderer → ventana.
struct Session {
    current: Option<MailFrame>,
    mailbox: Arc<Mailbox>,
    decode_thread: Option<JoinHandle<()>>,
    renderer: Renderer,
    window: Window,
    stats: Arc<Mutex<SessionStats>>,
    /// Hay un frame de vídeo nuevo en el buzón: presentar en cuanto haya hueco.
    dirty_video: bool,
    /// Solo cambió el cursor (o hay que repintar): presentar en
    /// `cursor_deadline`, justo antes del *vblank*, para que un frame que
    /// llegue entretanto salga en el mismo `Present` en vez de esperar al
    /// siguiente hueco de la cadena (con latencia máxima 1, cada `Present`
    /// ocupa el hueco hasta el *vblank*).
    cursor_deadline: Option<Instant>,
    screenshot_done: bool,
    /// Instante del último `Present` con un frame de vídeo nuevo (para
    /// medir congelaciones).
    last_video_present: Option<Instant>,
    /// Posición inicial del cursor local al abrir la ventana, para no teletransportar
    /// el ratón del emisor por el `WM_MOUSEMOVE` sintético de creación de ventana.
    initial_local_cursor: Option<(f64, f64)>,
    /// `true` en cuanto el usuario mueve o pulsa físicamente el ratón en el receptor.
    local_mouse_engaged: bool,
}

impl Session {
    fn mark_cursor_dirty(&mut self, refresh: Duration) {
        if self.cursor_deadline.is_some() {
            return;
        }
        let wait = self
            .renderer
            .time_to_next_vblank(refresh)
            .map(|t| t.saturating_sub(CURSOR_LATCH_MARGIN))
            .unwrap_or_default();
        self.cursor_deadline = Some(Instant::now() + wait);
    }

    fn due(&self, now: Instant) -> bool {
        self.dirty_video || self.cursor_deadline.is_some_and(|d| now >= d)
    }
}

struct ListenApp {
    args: Args,
    proxy: EventLoopProxy<UserEvent>,
    device: ID3D11Device,
    net: Option<JoinHandle<()>>,
    net_cmd: Option<UnboundedSender<UiCmd>>,
    monitor: Option<MonitorHandle>,
    session: Option<Session>,
    deadline: Option<Instant>,
    shutdown_at: Option<Instant>,
    /// Periodo de refresco del monitor elegido.
    refresh: Duration,
    error: Option<anyhow::Error>,
}

impl ListenApp {
    /// Elige el monitor, calcula `native_mode`/dpi y arranca la red.
    fn init(&mut self, el: &ActiveEventLoop) -> Result<()> {
        let monitors: Vec<_> = el.available_monitors().collect();
        for (i, m) in monitors.iter().enumerate() {
            println!(
                "monitor {i}: {} {}x{} en ({}, {}), {:.2} Hz, escala {:.2}",
                m.name().unwrap_or_default(),
                m.size().width,
                m.size().height,
                m.position().x,
                m.position().y,
                f64::from(m.refresh_rate_millihertz().unwrap_or(0)) / 1000.0,
                m.scale_factor()
            );
        }
        let monitor = match self.args.monitor {
            Some(n) => Some(
                monitors
                    .get(n)
                    .cloned()
                    .ok_or_else(|| anyhow!("no existe el monitor {n}"))?,
            ),
            None => el.primary_monitor().or_else(|| monitors.first().cloned()),
        };
        let refresh_mhz = monitor
            .as_ref()
            .and_then(MonitorHandle::refresh_rate_millihertz)
            .unwrap_or(60_000);
        let dpi = monitor
            .as_ref()
            .map(|m| (m.scale_factor() * 96.0).round() as u16)
            .unwrap_or(96);
        // Contrato: a pantalla completa, la resolución del monitor; en
        // ventana, la de `--size` o 1920×1080.
        let native_mode = match (&monitor, self.args.fullscreen) {
            (Some(m), true) => Mode {
                width: m.size().width,
                height: m.size().height,
                refresh_mhz,
            },
            _ => {
                let (w, h) = parse_size(self.args.size.as_deref().unwrap_or("1920x1080"))?;
                Mode {
                    width: w,
                    height: h,
                    refresh_mhz,
                }
            }
        };
        self.monitor = monitor;
        self.refresh = Duration::from_secs_f64(1000.0 / f64::from(refresh_mhz.max(1_000)));
        let data_dir = match &self.args.data_dir {
            Some(d) => d.clone(),
            None => directories::ProjectDirs::from("org", "CrossScreen", "CrossScreen")
                .context("no se pudo determinar el directorio de datos")?
                .data_dir()
                .to_path_buf(),
        };
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = NetConfig {
            bind: self.args.listen.ok_or_else(|| anyhow!("falta --listen"))?,
            data_dir,
            once: self.args.once,
            native_mode,
            dpi,
            keyframe_policy: self.args.keyframe_policy,
            clipboard: self.args.clipboard,
        };
        self.net = Some(net::spawn(cfg, self.proxy.clone(), cmd_rx)?);
        self.net_cmd = Some(cmd_tx);
        self.deadline = self
            .args
            .seconds
            .map(|s| Instant::now() + Duration::from_secs_f64(s));
        Ok(())
    }

    fn start_session(&mut self, el: &ActiveEventLoop, s: StartSession) -> Result<()> {
        // Por si quedó una sesión anterior (no debería: la red las serializa).
        self.end_session("sesión nueva");
        let (w, h) = parse_size(self.args.size.as_deref().unwrap_or("1920x1080"))?;
        let mut attrs = Window::default_attributes()
            .with_title(format!("CrossScreen — {}", s.peer))
            .with_inner_size(PhysicalSize::new(w, h));
        if self.args.fullscreen {
            attrs = attrs
                .with_decorations(false)
                .with_fullscreen(Some(Fullscreen::Borderless(self.monitor.clone())));
        } else if let Some(m) = &self.monitor {
            let p = m.position();
            attrs = attrs.with_position(PhysicalPosition::new(p.x + 40, p.y + 40));
        }
        let window = el.create_window(attrs)?;
        let hwnd = match window.window_handle()?.as_raw() {
            RawWindowHandle::Win32(h) => windows::Win32::Foundation::HWND(h.hwnd.get() as *mut _),
            other => return Err(anyhow!("handle de ventana inesperado: {other:?}")),
        };
        let size = window.inner_size();
        let mut renderer = Renderer::new(
            &self.device,
            hwnd,
            size.width,
            size.height,
            RendererOptions {
                vsync: !self.args.no_vsync,
            },
        )?;
        // Sin posición todavía: cursor oculto. Hasta la primera forma se usa
        // la flecha procedimental de `cs-render`.
        renderer.set_cursor((0.0, 0.0), false);
        let mailbox = Arc::new(Mailbox::default());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let worker = Worker {
            device: self.device.clone(),
            initial_size: Some((s.display_mode.width, s.display_mode.height)),
            input: s.decode_rx,
            mailbox: mailbox.clone(),
            proxy: self.proxy.clone(),
            keyframe_tx: s.keyframe_tx,
            stats: s.stats.clone(),
            policy: if self.args.freeze_on_loss {
                LossPolicy::FreezeOnLoss
            } else {
                LossPolicy::DecodeThrough
            },
        };
        let decode_thread = std::thread::Builder::new()
            .name("cs-decode".into())
            .spawn(move || decode_worker::run(worker, ready_tx))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(anyhow!(e)),
            Err(_) => return Err(anyhow!("el decodificador no arrancó a tiempo")),
        }
        println!(
            "sesión con {}: ventana {}x{}, modo del emisor {}x{}",
            s.peer, size.width, size.height, s.display_mode.width, s.display_mode.height
        );
        self.session = Some(Session {
            current: None,
            mailbox,
            decode_thread: Some(decode_thread),
            renderer,
            window,
            stats: s.stats,
            dirty_video: false,
            // Primer repintado: muestra la ventana en negro.
            cursor_deadline: Some(Instant::now()),
            screenshot_done: false,
            last_video_present: None,
            initial_local_cursor: None,
            local_mouse_engaged: false,
        });
        Ok(())
    }

    fn remote_input_enabled(&self) -> bool {
        !self.args.no_remote_input && !self.args.same_host_latency
    }

    fn send_input(&self, ev: InputEvent) {
        if let Some(tx) = &self.net_cmd {
            let _ = tx.send(UiCmd::Input(ev));
        }
    }

    fn end_session(&mut self, reason: &str) {
        let Some(mut s) = self.session.take() else {
            return;
        };
        s.current = None;
        drop(s.mailbox.take());
        if let Some(t) = s.decode_thread.take() {
            // La red ya soltó su extremo del canal: el hilo está terminando.
            let _ = t.join();
        }
        drop(s.mailbox.take());
        println!("fin de sesión: {reason}");
        s.stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .report(self.args.same_host_latency);
        drop(s.renderer);
        drop(s.window);
    }

    fn request_shutdown(&mut self, el: &ActiveEventLoop) {
        if self.shutdown_at.is_some() {
            return;
        }
        self.shutdown_at = Some(Instant::now());
        let sent = self
            .net_cmd
            .as_ref()
            .is_some_and(|tx| tx.send(UiCmd::Shutdown).is_ok());
        if !sent {
            el.exit();
        }
    }

    /// Espera al *waitable*, toma el frame más reciente (si lo hay) y
    /// presenta vídeo + cursor.
    fn render_once(&mut self) -> Result<()> {
        let same_host = self.args.same_host_latency;
        let shot_frame = self.args.screenshot_frame;
        let shot_concealed = self.args.screenshot_concealed;
        let shot_path = self.args.screenshot.clone();
        let Some(s) = self.session.as_mut() else {
            return Ok(());
        };
        s.dirty_video = false;
        s.cursor_deadline = None;
        s.renderer.wait_for_frame(WAITABLE_TIMEOUT);
        // Después de esperar: así se coge el frame más reciente posible.
        let new = s.mailbox.take();
        let is_new = new.is_some();
        if let Some(f) = new {
            // El frame anterior se suelta aquí y su superficie vuelve al MFT.
            s.current = Some(f);
        }
        let frame_ref = s.current.as_ref().map(|m| FrameRef {
            texture: &m.frame.texture,
            subresource: m.frame.subresource,
            width: m.frame.width,
            height: m.frame.height,
        });
        s.renderer.render(frame_ref)?;
        let presented_idx = s
            .stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .frames_presented;
        // Con `--screenshot-concealed`, el primer frame con referencias rotas
        // a partir del número `--screenshot-frame` (para ver la ocultación).
        let shot_now = if shot_concealed {
            presented_idx >= shot_frame && s.current.as_ref().is_some_and(|m| m.concealed)
        } else {
            presented_idx == shot_frame
        };
        if is_new && !s.screenshot_done && shot_now {
            if let Some(path) = &shot_path {
                let (w, h, rgba) = s.renderer.capture_rgba()?;
                save_png(path, w, h, &rgba)?;
                println!(
                    "captura del back buffer ({w}x{h}, frame presentado {presented_idx}) guardada en {}",
                    path.display()
                );
            }
            s.screenshot_done = true;
        }
        let ps = s.renderer.present()?;
        let now = Instant::now();
        let now_q = clock::qpc_us32();
        let mut st = s.stats.lock().unwrap_or_else(|e| e.into_inner());
        st.present_cpu.record(ps.cpu_submit);
        if let Some(v) = ps.present_to_vblank {
            st.present_vblank.record(v);
        }
        st.zero_copy = s.renderer.zero_copy();
        match s.current.as_ref().filter(|_| is_new) {
            Some(m) => {
                st.frames_presented += 1;
                if m.concealed {
                    st.frames_presented_concealed += 1;
                }
                if let Some(prev) = s.last_video_present {
                    let gap = now.duration_since(prev);
                    if gap > FREEZE_THRESHOLD {
                        st.freezes += 1;
                        st.frozen_total += gap;
                        st.longest_freeze = st.longest_freeze.max(gap);
                    }
                }
                s.last_video_present = Some(now);
                st.present.record(now.duration_since(m.decoded_at));
                st.recv_to_present.record(now.duration_since(m.received_at));
                if same_host {
                    match clock::latency_since(m.pts_us, now_q) {
                        Some(d) => st.capture_to_present.record(d),
                        None => st.capture_to_present_invalid += 1,
                    }
                }
                tracing::trace!("frame {} presentado", m.frame_seq);
            }
            None => st.cursor_only_presents += 1,
        }
        Ok(())
    }
}

impl ApplicationHandler<UserEvent> for ListenApp {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.net.is_some() {
            return;
        }
        if let Err(e) = self.init(el) {
            self.error = Some(e);
            el.exit();
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Start(s, ready) => {
                let res = self.start_session(el, *s);
                if let Err(e) = &res {
                    eprintln!("no se pudo iniciar la sesión: {e:#}");
                    self.end_session("fallo al iniciar");
                }
                let _ = ready.send(res.map_err(|e| format!("{e:#}")));
            }
            UserEvent::CursorShape(c) => {
                if let Some(s) = self.session.as_mut() {
                    match s.renderer.set_cursor_shape(
                        u32::from(c.width),
                        u32::from(c.height),
                        (u32::from(c.hotspot_x), u32::from(c.hotspot_y)),
                        &c.rgba,
                    ) {
                        Ok(()) => s.mark_cursor_dirty(self.refresh),
                        Err(e) => tracing::warn!("forma de cursor: {e}"),
                    }
                }
            }
            UserEvent::CursorPosition { x, y, visible } => {
                if let Some(s) = self.session.as_mut() {
                    s.renderer.set_cursor((x as f32, y as f32), visible);
                    s.mark_cursor_dirty(self.refresh);
                }
            }
            UserEvent::FrameReady => {
                if let Some(s) = self.session.as_mut() {
                    s.dirty_video = true;
                }
            }
            UserEvent::End { reason } => self.end_session(&reason),
            UserEvent::NetDone(err) => {
                if let Some(e) = err {
                    self.error = Some(anyhow!(e));
                }
                self.end_session("el hilo de red terminó");
                self.net_cmd = None;
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.request_shutdown(el),
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                self.request_shutdown(el)
            }
            WindowEvent::KeyboardInput { event, .. } if self.remote_input_enabled() => {
                if self.session.is_some() {
                    if let PhysicalKey::Code(code) = event.physical_key {
                        if let Some((scancode, extended)) = keycode_to_scancode(code) {
                            let state = match event.state {
                                ElementState::Pressed => ButtonState::Pressed,
                                ElementState::Released => ButtonState::Released,
                            };
                            self.send_input(InputEvent::Key {
                                stream_id: net::STREAM_ID,
                                scancode,
                                extended,
                                state,
                            });
                        }
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } if self.remote_input_enabled() => {
                let norm = if let Some(s) = self.session.as_mut() {
                    if !s.local_mouse_engaged {
                        match s.initial_local_cursor {
                            None => {
                                s.initial_local_cursor = Some((position.x, position.y));
                                None
                            }
                            Some((x0, y0)) if (position.x - x0).hypot(position.y - y0) >= 3.0 => {
                                s.local_mouse_engaged = true;
                                let sz = s.window.inner_size();
                                Some((
                                    norm_coord(position.x, sz.width),
                                    norm_coord(position.y, sz.height),
                                ))
                            }
                            Some(_) => None,
                        }
                    } else {
                        let sz = s.window.inner_size();
                        Some((
                            norm_coord(position.x, sz.width),
                            norm_coord(position.y, sz.height),
                        ))
                    }
                } else {
                    None
                };
                if let Some((x_norm, y_norm)) = norm {
                    self.send_input(InputEvent::MouseMove {
                        stream_id: net::STREAM_ID,
                        x_norm,
                        y_norm,
                    });
                }
            }
            WindowEvent::MouseInput { state, button, .. } if self.remote_input_enabled() => {
                let first_engage_pos = if let Some(s) = self.session.as_mut() {
                    let pos = if !s.local_mouse_engaged {
                        s.local_mouse_engaged = true;
                        s.initial_local_cursor.map(|(x, y)| {
                            let sz = s.window.inner_size();
                            (norm_coord(x, sz.width), norm_coord(y, sz.height))
                        })
                    } else {
                        None
                    };
                    Some(pos)
                } else {
                    None
                };
                if let Some(pos) = first_engage_pos {
                    if let Some((x_norm, y_norm)) = pos {
                        self.send_input(InputEvent::MouseMove {
                            stream_id: net::STREAM_ID,
                            x_norm,
                            y_norm,
                        });
                    }
                    if let Some(button) = map_mouse_button(button) {
                        let state = match state {
                            ElementState::Pressed => ButtonState::Pressed,
                            ElementState::Released => ButtonState::Released,
                        };
                        self.send_input(InputEvent::MouseButton {
                            stream_id: net::STREAM_ID,
                            button,
                            state,
                        });
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } if self.remote_input_enabled() => {
                if self.session.is_some() {
                    let (dx, dy) = match delta {
                        MouseScrollDelta::LineDelta(lx, ly) => (
                            (lx * 120.0)
                                .round()
                                .clamp(f32::from(i16::MIN), f32::from(i16::MAX))
                                as i16,
                            (ly * 120.0)
                                .round()
                                .clamp(f32::from(i16::MIN), f32::from(i16::MAX))
                                as i16,
                        ),
                        MouseScrollDelta::PixelDelta(p) => (
                            p.x.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16,
                            p.y.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16,
                        ),
                    };
                    if dx != 0 || dy != 0 {
                        self.send_input(InputEvent::MouseWheel {
                            stream_id: net::STREAM_ID,
                            delta_x: dx,
                            delta_y: dy,
                        });
                    }
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(s) = self.session.as_mut() {
                    if let Err(e) = s.renderer.resize(size.width, size.height) {
                        self.error = Some(e.into());
                        self.request_shutdown(el);
                        return;
                    }
                    s.cursor_deadline = Some(Instant::now());
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(s) = self.session.as_mut() {
                    s.cursor_deadline.get_or_insert_with(Instant::now);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        if self.deadline.is_some_and(|d| now >= d) {
            self.request_shutdown(el);
        }
        if self
            .shutdown_at
            .is_some_and(|t| now.duration_since(t) >= SHUTDOWN_GRACE)
        {
            eprintln!("la red no terminó a tiempo; saliendo");
            el.exit();
            return;
        }
        if self.session.as_ref().is_some_and(|s| s.due(now)) {
            if let Err(e) = self.render_once() {
                eprintln!("error de presentación: {e:#}");
                self.error = Some(e);
                self.request_shutdown(el);
            }
        }
        let wake = [
            self.deadline,
            self.shutdown_at.map(|t| t + SHUTDOWN_GRACE),
            self.session.as_ref().and_then(|s| s.cursor_deadline),
        ]
        .into_iter()
        .flatten()
        .min();
        el.set_control_flow(match wake {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }
}

/// Convierte una coordenada local `0..size` en `0..=65535`.
fn norm_coord(pos: f64, size: u32) -> u16 {
    if size <= 1 {
        return 0;
    }
    let max_idx = f64::from(size - 1);
    let clamped = pos.clamp(0.0, max_idx);
    ((clamped * 65535.0 / max_idx).round() as u32).min(65535) as u16
}

fn map_mouse_button(b: WinitMouseButton) -> Option<ProtoMouseButton> {
    match b {
        WinitMouseButton::Left => Some(ProtoMouseButton::Left),
        WinitMouseButton::Right => Some(ProtoMouseButton::Right),
        WinitMouseButton::Middle => Some(ProtoMouseButton::Middle),
        WinitMouseButton::Back => Some(ProtoMouseButton::Back),
        WinitMouseButton::Forward => Some(ProtoMouseButton::Forward),
        WinitMouseButton::Other(_) => None,
    }
}

/// Traduce `winit::keyboard::KeyCode` a un *scan code* del conjunto 1 de PC
/// (`0x01..=0xFF`) e indicador de prefijo `E0` (`extended`).
fn keycode_to_scancode(code: KeyCode) -> Option<(u16, bool)> {
    use KeyCode::*;
    let pair = match code {
        Escape => (0x01, false),
        Digit1 => (0x02, false),
        Digit2 => (0x03, false),
        Digit3 => (0x04, false),
        Digit4 => (0x05, false),
        Digit5 => (0x06, false),
        Digit6 => (0x07, false),
        Digit7 => (0x08, false),
        Digit8 => (0x09, false),
        Digit9 => (0x0A, false),
        Digit0 => (0x0B, false),
        Minus => (0x0C, false),
        Equal => (0x0D, false),
        Backspace => (0x0E, false),
        Tab => (0x0F, false),
        KeyQ => (0x10, false),
        KeyW => (0x11, false),
        KeyE => (0x12, false),
        KeyR => (0x13, false),
        KeyT => (0x14, false),
        KeyY => (0x15, false),
        KeyU => (0x16, false),
        KeyI => (0x17, false),
        KeyO => (0x18, false),
        KeyP => (0x19, false),
        BracketLeft => (0x1A, false),
        BracketRight => (0x1B, false),
        Enter => (0x1C, false),
        ControlLeft => (0x1D, false),
        KeyA => (0x1E, false),
        KeyS => (0x1F, false),
        KeyD => (0x20, false),
        KeyF => (0x21, false),
        KeyG => (0x22, false),
        KeyH => (0x23, false),
        KeyJ => (0x24, false),
        KeyK => (0x25, false),
        KeyL => (0x26, false),
        Semicolon => (0x27, false),
        Quote => (0x28, false),
        Backquote => (0x29, false),
        ShiftLeft => (0x2A, false),
        Backslash => (0x2B, false),
        KeyZ => (0x2C, false),
        KeyX => (0x2D, false),
        KeyC => (0x2E, false),
        KeyV => (0x2F, false),
        KeyB => (0x30, false),
        KeyN => (0x31, false),
        KeyM => (0x32, false),
        Comma => (0x33, false),
        Period => (0x34, false),
        Slash => (0x35, false),
        ShiftRight => (0x36, false),
        NumpadMultiply => (0x37, false),
        AltLeft => (0x38, false),
        Space => (0x39, false),
        CapsLock => (0x3A, false),
        F1 => (0x3B, false),
        F2 => (0x3C, false),
        F3 => (0x3D, false),
        F4 => (0x3E, false),
        F5 => (0x3F, false),
        F6 => (0x40, false),
        F7 => (0x41, false),
        F8 => (0x42, false),
        F9 => (0x43, false),
        F10 => (0x44, false),
        NumLock => (0x45, false),
        ScrollLock => (0x46, false),
        Numpad7 => (0x47, false),
        Numpad8 => (0x48, false),
        Numpad9 => (0x49, false),
        NumpadSubtract => (0x4A, false),
        Numpad4 => (0x4B, false),
        Numpad5 => (0x4C, false),
        Numpad6 => (0x4D, false),
        NumpadAdd => (0x4E, false),
        Numpad1 => (0x4F, false),
        Numpad2 => (0x50, false),
        Numpad3 => (0x51, false),
        Numpad0 => (0x52, false),
        NumpadDecimal => (0x53, false),
        IntlBackslash => (0x56, false),
        F11 => (0x57, false),
        F12 => (0x58, false),
        NumpadEnter => (0x1C, true),
        ControlRight => (0x1D, true),
        NumpadDivide => (0x35, true),
        PrintScreen => (0x37, true),
        AltRight => (0x38, true),
        Home => (0x47, true),
        ArrowUp => (0x48, true),
        PageUp => (0x49, true),
        ArrowLeft => (0x4B, true),
        ArrowRight => (0x4D, true),
        End => (0x4F, true),
        ArrowDown => (0x50, true),
        PageDown => (0x51, true),
        Insert => (0x52, true),
        Delete => (0x53, true),
        SuperLeft => (0x5B, true),
        SuperRight => (0x5C, true),
        ContextMenu => (0x5D, true),
        _ => return None,
    };
    Some(pair)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_coord_maps_edges_and_center() {
        assert_eq!(norm_coord(0.0, 1366), 0);
        assert_eq!(norm_coord(1365.0, 1366), 65535);
        assert_eq!(norm_coord(-10.0, 1366), 0);
        assert_eq!(norm_coord(9999.0, 1366), 65535);
        assert_eq!(norm_coord(959.5, 1920), 32768);
    }

    #[test]
    fn keycode_scancodes_match_pc_set1() {
        assert_eq!(keycode_to_scancode(KeyCode::KeyA), Some((0x1E, false)));
        assert_eq!(keycode_to_scancode(KeyCode::Enter), Some((0x1C, false)));
        assert_eq!(
            keycode_to_scancode(KeyCode::NumpadEnter),
            Some((0x1C, true))
        );
        assert_eq!(keycode_to_scancode(KeyCode::ArrowLeft), Some((0x4B, true)));
        assert_eq!(
            keycode_to_scancode(KeyCode::ControlLeft),
            Some((0x1D, false))
        );
        assert_eq!(
            keycode_to_scancode(KeyCode::ControlRight),
            Some((0x1D, true))
        );
    }
}
