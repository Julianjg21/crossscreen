//! Bench de la PoC-2: captura DXGI Desktop Duplication del monitor virtual.
//!
//! ```text
//! cargo run -p cs-capture --release --example capture_bench -- \
//!     [--target virtual|primary|\\.\DISPLAYn] [--seconds 10] \
//!     [--mode-change-at 5] [--no-animate] [--no-gpu-wait] \
//!     [--save-bmp DIR] [--exe RUTA\crossscreen.exe]
//! ```
//!
//! Con `--target virtual` (por defecto) muestra el monitor virtual con
//! `crossscreen display show` y lo retira al terminar (también si hay error).
//! Una ventana propia anima el contenido de la salida capturada para que
//! haya frames nuevos; sin `--no-animate` en una salida estática solo habría
//! *timeouts*.
#![allow(unsafe_code)] // Ventana Win32 de animación; cada bloque unsafe va comentado.

#[cfg(not(windows))]
fn main() {
    eprintln!("capture_bench solo funciona en Windows");
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    match win::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
mod win {
    use cs_capture::bmp::{bgra_content_summary, encode_bgra_bmp};
    use cs_capture::stats::Samples;
    use cs_capture::windows_dxgi::{list_outputs, D3D11Texture, DxgiCapture};
    use cs_capture::{CaptureConfig, CaptureSource, CaptureTarget, PixelFormat, Rect};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Dwm::DwmFlush;
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;

    type BoxErr = Box<dyn std::error::Error>;

    struct Args {
        target: String,
        seconds: f64,
        mode_change_at: Option<f64>,
        animate: bool,
        gpu_wait: bool,
        save_bmp: Option<PathBuf>,
        exe: PathBuf,
    }

    fn parse_args() -> Result<Args, BoxErr> {
        let mut a = Args {
            target: "virtual".into(),
            seconds: 10.0,
            mode_change_at: None,
            animate: true,
            gpu_wait: true,
            save_bmp: None,
            exe: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/release/crossscreen.exe"),
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut val = || it.next().ok_or_else(|| format!("falta el valor de {arg}"));
            match arg.as_str() {
                "--target" => a.target = val()?,
                "--seconds" => a.seconds = val()?.parse()?,
                "--mode-change-at" => a.mode_change_at = Some(val()?.parse()?),
                "--no-animate" => a.animate = false,
                "--no-gpu-wait" => a.gpu_wait = false,
                "--save-bmp" => a.save_bmp = Some(val()?.into()),
                "--exe" => a.exe = val()?.into(),
                other => return Err(format!("argumento desconocido: {other}").into()),
            }
        }
        Ok(a)
    }

    /// Muestra el monitor virtual y lo retira en Drop (también con `?` o pánico).
    struct VirtualMonitor {
        exe: PathBuf,
        gdi_name: String,
    }

    impl VirtualMonitor {
        fn show(exe: &PathBuf, w: u32, h: u32) -> Result<Self, BoxErr> {
            let out = Command::new(exe)
                .args(["display", "show", "--width", &w.to_string()])
                .args(["--height", &h.to_string(), "--hz", "60"])
                .output()?;
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            print!("{stdout}");
            if !out.status.success() {
                let _ = Command::new(exe).args(["display", "hide"]).status();
                return Err(format!(
                    "display show falló: {}",
                    String::from_utf8_lossy(&out.stderr)
                )
                .into());
            }
            let gdi_name = stdout
                .split_whitespace()
                .find(|t| t.starts_with(r"\\.\DISPLAY"))
                .ok_or("no se pudo leer el nombre GDI de la salida de display show")?
                .to_string();
            Ok(Self {
                exe: exe.clone(),
                gdi_name,
            })
        }
    }

    impl Drop for VirtualMonitor {
        fn drop(&mut self) {
            match Command::new(&self.exe).args(["display", "hide"]).output() {
                Ok(o) => print!("{}", String::from_utf8_lossy(&o.stdout)),
                Err(e) => eprintln!("¡no se pudo retirar el monitor virtual!: {e}"),
            }
        }
    }

    // ---------- Ventana de animación ----------

    struct Animator {
        stop: Arc<AtomicBool>,
        paints: Arc<AtomicU64>,
        rect: Arc<Mutex<Rect>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Animator {
        fn start(rect: Rect) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let paints = Arc::new(AtomicU64::new(0));
            let shared = Arc::new(Mutex::new(rect));
            let (s, p, r) = (stop.clone(), paints.clone(), shared.clone());
            let thread = std::thread::spawn(move || animate(s, p, r));
            Self {
                stop,
                paints,
                rect: shared,
                thread: Some(thread),
            }
        }

        fn move_to(&self, r: Rect) {
            if let Ok(mut g) = self.rect.lock() {
                *g = r;
            }
        }
    }

    impl Drop for Animator {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        // SAFETY: reenvía el mensaje sin modificar al procedimiento por defecto.
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    fn animate(stop: Arc<AtomicBool>, paints: Arc<AtomicU64>, rect: Arc<Mutex<Rect>>) {
        // SAFETY: API Win32 de ventanas y GDI usada desde un único hilo dueño
        // de la ventana; todos los handles se liberan antes de salir.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let Ok(hinst) = GetModuleHandleW(PCWSTR::null()) else {
                return;
            };
            let class = w!("CrossScreenCaptureBench");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let mut cur = rect.lock().map(|g| *g).unwrap_or_default();
            let Ok(hwnd) = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class,
                w!("CrossScreen capture bench"),
                WS_POPUP,
                cur.left,
                cur.top,
                cur.width(),
                cur.height(),
                None,
                None,
                hinst,
                None,
            ) else {
                eprintln!("no se pudo crear la ventana de animación");
                return;
            };
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let mut wr = RECT::default();
            if GetWindowRect(hwnd, &mut wr).is_ok() {
                println!(
                    "ventana de animación en ({},{})-({},{})",
                    wr.left, wr.top, wr.right, wr.bottom
                );
            }
            let mut n: u64 = 0;
            let mut msg = MSG::default();
            while !stop.load(Ordering::Relaxed) {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                let want = rect.lock().map(|g| *g).unwrap_or(cur);
                if want != cur {
                    cur = want;
                    let _ = SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        cur.left,
                        cur.top,
                        cur.width(),
                        cur.height(),
                        SWP_NOACTIVATE,
                    );
                }
                let (w, h) = (cur.width(), cur.height());
                let hdc = GetDC(hwnd);
                // Fondo que cambia de color en cada pintado y una barra
                // blanca que recorre la ventana: todo frame es distinto.
                let c = (n * 7 % 256) as u32;
                let bg = CreateSolidBrush(COLORREF(c | ((255 - c) << 8) | (((c * 3) % 256) << 16)));
                let all = RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                };
                FillRect(hdc, &all, bg);
                let _ = DeleteObject(bg);
                let x = ((n * 16) % (w.max(1) as u64)) as i32;
                let bar = RECT {
                    left: x,
                    top: 0,
                    right: x + 40,
                    bottom: h,
                };
                FillRect(hdc, &bar, HBRUSH(GetStockObject(WHITE_BRUSH).0));
                let text: Vec<u16> = format!("CrossScreen PoC-2  frame {n}")
                    .encode_utf16()
                    .collect();
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, COLORREF(0));
                let _ = TextOutW(hdc, 20, 20, &text);
                ReleaseDC(hwnd, hdc);
                n += 1;
                paints.store(n, Ordering::Relaxed);
                // Ritmo del compositor (vsync de DWM).
                if DwmFlush().is_err() {
                    std::thread::sleep(Duration::from_millis(16));
                }
            }
            let _ = DestroyWindow(hwnd);
        }
    }

    // ---------- Bench ----------

    fn ms(us: Option<f64>) -> String {
        us.map(|v| format!("{:.3}", v / 1000.0))
            .unwrap_or_else(|| "-".into())
    }

    fn save_bmp(cap: &DxgiCapture, tex: &D3D11Texture, path: &PathBuf) -> Result<(), BoxErr> {
        let img = cap.read_to_cpu(tex)?;
        if img.format != PixelFormat::Bgra8 {
            return Err(format!("formato {:?}: solo se vuelca BGRA8", img.format).into());
        }
        let (luma, colors) = bgra_content_summary(img.width, img.height, img.stride, &img.data);
        let bmp = encode_bgra_bmp(img.width, img.height, img.stride, &img.data)
            .ok_or("dimensiones inválidas")?;
        std::fs::write(path, bmp)?;
        println!(
            "  BMP {} ({}x{}): luminancia media {luma:.1}, {colors} colores distintos en la muestra",
            path.display(),
            img.width,
            img.height
        );
        Ok(())
    }

    pub fn run() -> Result<(), BoxErr> {
        let args = parse_args()?;
        // SAFETY: fija la conciencia DPI del proceso antes de crear ventanas,
        // para trabajar en píxeles físicos (panel al 125 %).
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }

        let mut _virtual = None;
        let target = match args.target.as_str() {
            "virtual" => {
                let t0 = Instant::now();
                let vm = VirtualMonitor::show(&args.exe, 1920, 1080)?;
                println!(
                    "monitor virtual {} visible en {:?}",
                    vm.gdi_name,
                    t0.elapsed()
                );
                let t = CaptureTarget::GdiName(vm.gdi_name.clone());
                _virtual = Some(vm);
                t
            }
            "primary" => CaptureTarget::Primary,
            name => CaptureTarget::GdiName(name.to_string()),
        };

        println!("Salidas DXGI:");
        for o in list_outputs()? {
            println!(
                "  {:<14} adaptador=\"{}\" luid={:#x} rect=({},{})-({},{}) escritorio={}",
                o.gdi_name,
                o.adapter_name,
                o.adapter_luid,
                o.desktop_rect.left,
                o.desktop_rect.top,
                o.desktop_rect.right,
                o.desktop_rect.bottom,
                o.attached_to_desktop
            );
        }

        let config = CaptureConfig {
            wait_for_gpu_copy: args.gpu_wait,
            ..Default::default()
        };
        let t_open = Instant::now();
        let mut cap = DxgiCapture::new(target.clone(), config)?;
        println!(
            "Captura abierta en {:?}: {} en \"{}\" -> {:?}",
            t_open.elapsed(),
            cap.output().gdi_name,
            cap.output().adapter_name,
            cap.duplication_info()
        );

        let anim_rect = |r: Rect| {
            if matches!(target, CaptureTarget::GdiName(_)) && args.target == "virtual" {
                r
            } else {
                // En el panel del usuario, solo una esquina pequeña.
                Rect {
                    left: r.left,
                    top: r.top,
                    right: r.left + 320,
                    bottom: r.top + 240,
                }
            }
        };
        let animator = args
            .animate
            .then(|| Animator::start(anim_rect(cap.output().desktop_rect)));
        // Dejar que la ventana aparezca antes de medir.
        std::thread::sleep(Duration::from_millis(300));
        // Descartar lo acumulado hasta ahora.
        while cap.next_frame(Duration::ZERO)?.is_some() {}

        let mut capture_us = Samples::new();
        let mut present_to_acquire_us = Samples::new();
        let mut changed = Samples::new();
        let mut per_second = vec![0u32; args.seconds.ceil() as usize + 1];
        let mut accumulated_gt1 = 0u64;
        let mut first_frame: Option<D3D11Texture> = None;
        let mut after_change_frame: Option<D3D11Texture> = None;
        let mut last_frame: Option<D3D11Texture> = None;
        let mut mode_thread: Option<std::thread::JoinHandle<String>> = None;
        let mut mode_cmd_start: Option<Instant> = None;
        let mut first_new_size: Option<Duration> = None;
        let mut sizes_seen: Vec<(u32, u32)> = Vec::new();
        let stats0 = cap.stats().clone();
        let cursor0 = cap.cursor().clone();
        let paints0 = animator
            .as_ref()
            .map(|a| a.paints.load(Ordering::Relaxed))
            .unwrap_or(0);

        let start = Instant::now();
        let run_for = Duration::from_secs_f64(args.seconds);
        while start.elapsed() < run_for {
            if let (Some(at), None, Some(vm)) = (args.mode_change_at, &mode_thread, &_virtual) {
                if start.elapsed().as_secs_f64() >= at {
                    let exe = vm.exe.clone();
                    mode_cmd_start = Some(Instant::now());
                    mode_thread = Some(std::thread::spawn(move || {
                        Command::new(exe)
                            .args(["display", "show", "--width", "1600", "--height", "1000"])
                            .args(["--hz", "60"])
                            .output()
                            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                            .unwrap_or_else(|e| format!("error: {e}"))
                    }));
                }
            }
            let Some(f) = cap.next_frame(Duration::from_millis(100))? else {
                continue;
            };
            let el = start.elapsed();
            capture_us.push(f.timing.capture_us() as f64);
            if let Some(p) = f.timing.present_to_acquire_us() {
                present_to_acquire_us.push(p as f64);
            }
            changed.push(f.update.changed_fraction(f.width, f.height));
            if f.accumulated_frames > 1 {
                accumulated_gt1 += 1;
            }
            if let Some(slot) = per_second.get_mut(el.as_secs() as usize) {
                *slot += 1;
            }
            if !sizes_seen.contains(&(f.width, f.height)) {
                sizes_seen.push((f.width, f.height));
            }
            last_frame = Some(f.texture.clone());
            if first_frame.is_none() && el > Duration::from_secs(1) {
                first_frame = Some(f.texture.clone());
            }
            if let Some(t0) = mode_cmd_start {
                if first_new_size.is_none() && (f.width, f.height) == (1600, 1000) {
                    first_new_size = Some(t0.elapsed());
                    // La ventana de animación sigue a la salida en su nueva posición.
                    if let Some(a) = &animator {
                        a.move_to(anim_rect(cap.output().desktop_rect));
                    }
                }
                if after_change_frame.is_none()
                    && first_new_size.is_some_and(|d| t0.elapsed() > d + Duration::from_millis(500))
                {
                    after_change_frame = Some(f.texture.clone());
                }
            }
        }
        let elapsed = start.elapsed().as_secs_f64();
        let paints = animator
            .as_ref()
            .map(|a| a.paints.load(Ordering::Relaxed) - paints0)
            .unwrap_or(0);
        drop(animator);
        let mode_out = mode_thread.map(|t| t.join().unwrap_or_default());

        let s = cap.stats().clone();
        let calls = s.calls - stats0.calls;
        let frames = s.frames - stats0.frames;
        let timeouts = s.timeouts - stats0.timeouts;
        println!();
        println!(
            "=== Resultado ({}, {:.1} s) ===",
            cap.output().gdi_name,
            elapsed
        );
        println!(
            "espera GPU de la copia: {}",
            if args.gpu_wait {
                "sí (copia completada)"
            } else {
                "no (solo encolada)"
            }
        );
        println!(
            "frames: {frames} ({:.1} fps), llamadas: {calls}, timeouts de 100 ms: {timeouts} ({:.1} %), solo cursor: {}",
            frames as f64 / elapsed,
            100.0 * timeouts as f64 / calls.max(1) as f64,
            s.pointer_only_updates - stats0.pointer_only_updates
        );
        if args.animate {
            println!(
                "ventana de animación: {paints} pintados ({:.1}/s)",
                paints as f64 / elapsed
            );
        }
        println!("fps por segundo: {per_second:?}");
        println!(
            "Acquire→copia lista (ms): p50 {} p95 {} p99 {} máx {} (n={})",
            ms(capture_us.percentile(50.0)),
            ms(capture_us.percentile(95.0)),
            ms(capture_us.percentile(99.0)),
            ms(capture_us.max()),
            capture_us.len()
        );
        println!(
            "Presentación DWM→Acquire (ms): p50 {} p95 {} máx {}",
            ms(present_to_acquire_us.percentile(50.0)),
            ms(present_to_acquire_us.percentile(95.0)),
            ms(present_to_acquire_us.max())
        );
        println!(
            "frames con AccumulatedFrames>1: {accumulated_gt1}; fracción cambiada media (cota sup.): {:.3}",
            changed.mean().unwrap_or(0.0)
        );
        println!(
            "access_lost: {}, reinits: {}, recreaciones de dispositivo: {}, última recuperación: {:?}",
            s.access_lost - stats0.access_lost,
            s.reinits - stats0.reinits,
            s.device_recreations - stats0.device_recreations,
            s.last_recovery
        );
        println!(
            "intentos de recreación fallidos: {}, último error: {:?}, pérdida→recreada: {:?}",
            s.failed_reinits - stats0.failed_reinits,
            s.last_reinit_error,
            s.last_reinit_latency
        );
        println!("tamaños vistos: {sizes_seen:?}");
        if let Some(o) = mode_out {
            println!("cambio de modo: \"{o}\"");
            println!(
                "orden de cambio → primer frame 1600x1000: {:?}",
                first_new_size
            );
        }
        let c = cap.cursor();
        println!(
            "cursor: posición {:?} visible {} (cambios de posición {}, formas {}){}",
            c.position,
            c.visible,
            c.position_generation - cursor0.position_generation,
            c.shape_generation - cursor0.shape_generation,
            c.shape
                .as_ref()
                .map(|s| format!(", forma {:?} {}x{}", s.kind, s.width, s.visible_height()))
                .unwrap_or_default()
        );

        if let Some(dir) = &args.save_bmp {
            std::fs::create_dir_all(dir)?;
            let tag = cap.output().gdi_name.replace(['\\', '.'], "");
            if let Some(t) = &first_frame {
                save_bmp(&cap, t, &dir.join(format!("{tag}-inicio.bmp")))?;
            }
            if let Some(t) = &last_frame {
                save_bmp(&cap, t, &dir.join(format!("{tag}-final.bmp")))?;
            }
            if let Some(t) = &after_change_frame {
                save_bmp(&cap, t, &dir.join(format!("{tag}-tras-cambio.bmp")))?;
            }
        }
        Ok(())
    }
}
