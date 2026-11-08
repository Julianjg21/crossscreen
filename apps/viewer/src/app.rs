//! Modo `--file` del visor (PoC-5): winit para la ventana, `cs-decode` +
//! `cs-render` sobre un único dispositivo D3D11, reproduciendo un fichero.

use crate::{parse_size, save_png, Args};
use anyhow::{anyhow, Context, Result};
use cs_decode::annexb::split_access_units;
use cs_decode::stats::LatencyStats;
use cs_decode::{create_d3d11_device, VideoDecoder};
use cs_render::{FrameRef, Renderer, RendererOptions};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Fullscreen, Window, WindowId};

/// Modo `--file`: reproduce un fichero Annex-B.
pub fn main(args: Args) -> Result<()> {
    let file = args.file.clone().ok_or_else(|| anyhow!("falta --file"))?;
    let data = std::fs::read(&file).with_context(|| format!("leyendo {}", file.display()))?;
    let aus: Vec<Vec<u8>> = split_access_units(&data)
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect();
    if aus.is_empty() {
        return Err(anyhow!("{} no contiene NAL units Annex-B", file.display()));
    }
    println!(
        "{}: {} bytes, {} access units",
        file.display(),
        data.len(),
        aus.len()
    );
    if args.fps <= 0.0 {
        return Err(anyhow!("--fps debe ser > 0"));
    }

    let event_loop = EventLoop::new()?;
    let mut app = App::new(args, aus);
    event_loop.run_app(&mut app)?;
    app.report();
    match app.error.take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

struct Gpu {
    // Orden de destrucción: frames → decodificador → renderer → ventana.
    decoder: VideoDecoder,
    renderer: Renderer,
    _window: Window,
}

#[derive(Default)]
struct Stats {
    decode_cpu: LatencyStats,
    decode_gpu: LatencyStats,
    render_gpu: LatencyStats,
    present_cpu: LatencyStats,
    present_vblank: LatencyStats,
    au_to_present: LatencyStats,
    au_to_present_no_wait: LatencyStats,
    wait_swapchain: LatencyStats,
    frames_in: u64,
    frames_out: u64,
    late_ticks: u64,
    errors: u64,
}

struct App {
    args: Args,
    aus: Vec<Vec<u8>>,
    gpu: Option<Gpu>,
    interval: Duration,
    next_tick: Instant,
    started: Option<Instant>,
    idx: usize,
    stats: Stats,
    error: Option<anyhow::Error>,
    zero_copy: Option<bool>,
    video_size: (u32, u32),
    stream_changes: u32,
}

impl App {
    fn new(args: Args, aus: Vec<Vec<u8>>) -> Self {
        let interval = Duration::from_secs_f64(1.0 / args.fps);
        Self {
            args,
            aus,
            gpu: None,
            interval,
            next_tick: Instant::now(),
            started: None,
            idx: 0,
            stats: Stats::default(),
            error: None,
            zero_copy: None,
            video_size: (0, 0),
            stream_changes: 0,
        }
    }

    fn init(&mut self, el: &ActiveEventLoop) -> Result<()> {
        let monitors: Vec<_> = el.available_monitors().collect();
        for (i, m) in monitors.iter().enumerate() {
            println!(
                "monitor {i}: {} {}x{} en ({}, {})",
                m.name().unwrap_or_default(),
                m.size().width,
                m.size().height,
                m.position().x,
                m.position().y
            );
        }
        let monitor = match self.args.monitor {
            Some(n) => Some(
                monitors
                    .get(n)
                    .cloned()
                    .ok_or_else(|| anyhow!("no existe el monitor {n}"))?,
            ),
            None => el.primary_monitor(),
        };
        let (w, h) = parse_size(self.args.size.as_deref().unwrap_or("1280x720"))?;
        let mut attrs = Window::default_attributes()
            .with_title("CrossScreen viewer")
            .with_inner_size(PhysicalSize::new(w, h));
        if self.args.fullscreen {
            attrs = attrs
                .with_decorations(false)
                .with_fullscreen(Some(Fullscreen::Borderless(monitor.clone())));
        } else if let Some(m) = &monitor {
            let p = m.position();
            attrs = attrs.with_position(PhysicalPosition::new(p.x + 40, p.y + 40));
        }
        let window = el.create_window(attrs)?;
        let hwnd = match window.window_handle()?.as_raw() {
            RawWindowHandle::Win32(h) => windows::Win32::Foundation::HWND(h.hwnd.get() as *mut _),
            other => return Err(anyhow!("handle de ventana inesperado: {other:?}")),
        };
        let size = window.inner_size();
        let device = create_d3d11_device()?;
        let renderer = Renderer::new(
            &device,
            hwnd,
            size.width,
            size.height,
            RendererOptions {
                vsync: !self.args.no_vsync,
            },
        )?;
        let mut decoder = VideoDecoder::new(&device)?;
        decoder.set_measure_gpu(!self.args.no_gpu_wait);
        self.gpu = Some(Gpu {
            decoder,
            renderer,
            _window: window,
        });
        let now = Instant::now();
        self.started = Some(now);
        self.next_tick = now;
        Ok(())
    }

    fn finished(&self) -> bool {
        match (self.args.seconds, self.started) {
            (Some(s), Some(t0)) => t0.elapsed().as_secs_f64() >= s,
            (None, _) => self.idx >= self.aus.len(),
            _ => false,
        }
    }

    /// Un *tick* de reproducción: decodifica la siguiente AU y la presenta.
    fn tick(&mut self) -> Result<()> {
        let Some(gpu) = self.gpu.as_mut() else {
            return Ok(());
        };
        let au = &self.aus[self.idx % self.aus.len()];
        let pts_us = (self.idx as f64 * 1e6 / self.args.fps) as i64;
        self.idx += 1;
        self.stats.frames_in += 1;
        let t_au = Instant::now();
        let frame = match gpu.decoder.decode(au, pts_us) {
            Ok(f) => f,
            Err(e) => {
                self.stats.errors += 1;
                tracing::warn!("decode AU {}: {e}", self.idx - 1);
                None
            }
        };
        self.stream_changes = gpu.decoder.stream_changes();
        let Some(frame) = frame else { return Ok(()) };
        self.stats.decode_cpu.record(frame.decode_time);
        if let Some(g) = frame.gpu_done_time {
            self.stats.decode_gpu.record(g);
        }
        self.video_size = (frame.width, frame.height);

        let tw = Instant::now();
        gpu.renderer.wait_for_frame(Duration::from_millis(100));
        let waited = tw.elapsed();
        self.stats.wait_swapchain.record(waited);

        if !self.args.no_cursor {
            // Cursor de demostración en círculo (en producción llega fuera de banda).
            let t = self.stats.frames_out as f32 / 60.0;
            let (vw, vh) = (frame.width as f32, frame.height as f32);
            let pos = (vw * 0.5 + vw * 0.3 * t.cos(), vh * 0.5 + vh * 0.3 * t.sin());
            gpu.renderer.set_cursor(pos, true);
        }
        let tr = Instant::now();
        gpu.renderer.render(Some(FrameRef {
            texture: &frame.texture,
            subresource: frame.subresource,
            width: frame.width,
            height: frame.height,
        }))?;
        if !self.args.no_gpu_wait {
            gpu.renderer.wait_gpu();
            self.stats.render_gpu.record(tr.elapsed());
        }
        if self.args.screenshot.is_some() && self.stats.frames_out == self.args.screenshot_frame {
            if let Some(path) = &self.args.screenshot {
                let (w, h, rgba) = gpu.renderer.capture_rgba()?;
                save_png(path, w, h, &rgba)?;
                println!(
                    "captura del back buffer ({w}x{h}) guardada en {}",
                    path.display()
                );
            }
        }
        let ps = gpu.renderer.present()?;
        self.stats.present_cpu.record(ps.cpu_submit);
        if let Some(v) = ps.present_to_vblank {
            self.stats.present_vblank.record(v);
        }
        let total = t_au.elapsed();
        if total > Duration::from_millis(50) {
            tracing::warn!(
                "frame {} lento: total {:.1} ms (render+GPU {:.1} ms, Present {:.1} ms)",
                self.stats.frames_out,
                total.as_secs_f64() * 1e3,
                tr.elapsed().as_secs_f64() * 1e3,
                ps.cpu_submit.as_secs_f64() * 1e3
            );
        }
        self.stats.au_to_present.record(total);
        self.stats
            .au_to_present_no_wait
            .record(total.saturating_sub(waited));
        self.stats.frames_out += 1;
        self.zero_copy = gpu.renderer.zero_copy();
        drop(frame);
        Ok(())
    }

    fn report(&self) {
        let elapsed = self
            .started
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        println!("=== crossscreen-viewer: resumen ===");
        println!(
            "vídeo {}x{}, {} AUs entregadas, {} frames presentados en {:.2} s ({:.1} fps), ticks tardíos {}, errores {}, stream changes {}",
            self.video_size.0,
            self.video_size.1,
            self.stats.frames_in,
            self.stats.frames_out,
            elapsed,
            if elapsed > 0.0 { self.stats.frames_out as f64 / elapsed } else { 0.0 },
            self.stats.late_ticks,
            self.stats.errors,
            self.stream_changes,
        );
        println!(
            "zero-copy (SRV sobre la superficie del decodificador): {}",
            match self.zero_copy {
                Some(true) => "sí",
                Some(false) => "no (copia GPU→GPU)",
                None => "sin frames",
            }
        );
        let line = |name: &str, s: &LatencyStats| match s.summary() {
            Some(sum) => println!("{name:<38} {sum}"),
            None => println!("{name:<38} (sin datos)"),
        };
        line("decode (AU→salida del MFT, CPU)", &self.stats.decode_cpu);
        line("decode (AU→GPU terminó)", &self.stats.decode_gpu);
        line("render NV12→RGB + cursor (GPU)", &self.stats.render_gpu);
        line("espera de la cadena (waitable)", &self.stats.wait_swapchain);
        line("Present() CPU", &self.stats.present_cpu);
        line("Present()→vblank (DXGI stats)", &self.stats.present_vblank);
        line("AU→Present() devuelto (total)", &self.stats.au_to_present);
        line(
            "AU→Present() sin la espera de vsync",
            &self.stats.au_to_present_no_wait,
        );
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        if let Err(e) = self.init(el) {
            self.error = Some(e);
            el.exit();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                el.exit()
            }
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    if let Err(e) = gpu.renderer.resize(size.width, size.height) {
                        self.error = Some(e.into());
                        el.exit();
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.gpu.is_none() {
            return;
        }
        if self.finished() {
            el.exit();
            return;
        }
        let now = Instant::now();
        if now >= self.next_tick {
            if let Err(e) = self.tick() {
                self.error = Some(e);
                el.exit();
                return;
            }
            self.next_tick += self.interval;
            // Si vamos más de un frame tarde, no intentar recuperar a ráfagas.
            if Instant::now() > self.next_tick + self.interval {
                self.stats.late_ticks += 1;
                self.next_tick = Instant::now();
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next_tick));
    }
}
