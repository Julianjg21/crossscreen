//! Banco de pruebas de la PoC-3: codificación H.264 por hardware (MF) con
//! entrada D3D11.
//!
//! Genera frames BGRA sintéticos **en la GPU** que cambian en cada frame
//! (fondo de color variable, cuatro bloques con textura detallada en
//! movimiento y una franja de ruido nueva por frame), los codifica a 60 fps
//! con ritmo real durante ~10 s y mide latencia por frame, fps, tasa de
//! salida, tamaño de IDR frente a P, latencia del IDR bajo demanda y el coste
//! de la conversión BGRA→NV12 con consultas de tiempo de la GPU.
//!
//! Uso: `cargo run -p cs-encode --example encode_bench --release -- [--quick] [--out DIR]`
#![allow(unsafe_code)] // FFI D3D11 del generador sintético y del temporizador.

#[cfg(not(windows))]
fn main() {
    eprintln!("encode_bench solo funciona en Windows (Media Foundation + D3D11)");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    bench::run()
}

#[cfg(windows)]
mod bench {
    use anyhow::{anyhow, Context};
    use cs_encode::annexb::{self, nal_type};
    use cs_encode::h264::{self, SliceType};
    use cs_encode::mf::{
        create_d3d11_device, enumerate_hw_h264_encoders, query_codec_property, Converter,
        D3D11Setup, InputFrame, MfH264Encoder,
    };
    use cs_encode::{EncodeControl, EncoderConfig, PropertyOutcome, PropertyReport, VideoEncoder};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use windows::core::{Interface, VARIANT};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Threading::{
        CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
    };

    const DEFAULT_OUT: &str =
        "C:/Users/JULIAN/AppData/Local/Temp/claude/9c55f4c2186a89c6/ee884e23-74ce-4658-b7d1-538bd77cf850/scratchpad/encoded";
    const PATTERN: u32 = 512;

    // ---------------------------------------------------------------- utilidades

    fn pct(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return f64::NAN;
        }
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    fn stats_ms(v: &[Duration]) -> (f64, f64, f64, f64) {
        let mut ms: Vec<f64> = v.iter().map(|d| d.as_secs_f64() * 1e3).collect();
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (
            pct(&ms, 0.5),
            pct(&ms, 0.95),
            pct(&ms, 0.99),
            *ms.last().unwrap_or(&f64::NAN),
        )
    }

    /// Temporizador de alta resolución para el ritmo de 60 fps sin quemar CPU
    /// (en un portátil con iGPU la CPU y la GPU comparten presupuesto térmico).
    struct Pacer {
        timer: HANDLE,
    }

    impl Pacer {
        fn new() -> anyhow::Result<Self> {
            // SAFETY: crea un temporizador anónimo; se cierra en Drop.
            let timer = unsafe {
                CreateWaitableTimerExW(
                    None,
                    None,
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS.0,
                )
            }?;
            Ok(Self { timer })
        }

        fn sleep_until(&self, t: Instant) {
            let now = Instant::now();
            if t <= now {
                return;
            }
            // Tiempo relativo negativo en unidades de 100 ns.
            let due = -((t - now).as_nanos() as i64 / 100);
            // SAFETY: handle válido; sin rutina APC.
            unsafe {
                if SetWaitableTimer(self.timer, &due, 0, None, None, false).is_ok() {
                    let r = WaitForSingleObject(self.timer, INFINITE);
                    debug_assert_eq!(r, WAIT_OBJECT_0);
                }
            }
        }
    }

    impl Drop for Pacer {
        fn drop(&mut self) {
            // SAFETY: handle propio.
            let _ = unsafe { CloseHandle(self.timer) };
        }
    }

    // ------------------------------------------------------ generador sintético

    struct Synth {
        context: ID3D11DeviceContext,
        frames: Vec<(ID3D11Texture2D, ID3D11RenderTargetView)>,
        pattern: ID3D11Texture2D,
        width: u32,
        height: u32,
        noise: Vec<u8>,
        rng: u64,
    }

    impl Synth {
        fn new(setup: &D3D11Setup, width: u32, height: u32) -> anyhow::Result<Self> {
            let device = &setup.device;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut frames = Vec::new();
            for _ in 0..3 {
                let mut tex = None;
                // SAFETY: descriptor válido; salida a Option local.
                unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }?;
                let tex = tex.ok_or_else(|| anyhow!("textura nula"))?;
                let mut rtv = None;
                // SAFETY: textura válida con BIND_RENDER_TARGET.
                unsafe { device.CreateRenderTargetView(&tex, None, Some(&mut rtv)) }?;
                frames.push((tex, rtv.ok_or_else(|| anyhow!("RTV nula"))?));
            }

            // Patrón 512×512: degradado, rejilla tipo "texto" y ruido.
            let mut rng = 0x1234_5678_9abc_def0u64;
            let mut px = vec![0u8; (PATTERN * PATTERN * 4) as usize];
            for y in 0..PATTERN {
                for x in 0..PATTERN {
                    let i = ((y * PATTERN + x) * 4) as usize;
                    rng = rng
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let n = (rng >> 56) as u8;
                    let glyph = ((x / 6) ^ (y / 10)) % 3 == 0 && (y % 10) < 7 && (x % 6) < 5;
                    let (b, g, r) = if y < PATTERN / 4 {
                        (n, n.wrapping_add(40), n.wrapping_mul(3))
                    } else if glyph {
                        (20, 20, 20)
                    } else {
                        ((x / 2) as u8, (y / 2) as u8, ((x + y) / 4) as u8)
                    };
                    px[i] = b;
                    px[i + 1] = g;
                    px[i + 2] = r;
                    px[i + 3] = 255;
                }
            }
            let pdesc = D3D11_TEXTURE2D_DESC {
                Width: PATTERN,
                Height: PATTERN,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..desc
            };
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: px.as_ptr() as *const _,
                SysMemPitch: PATTERN * 4,
                SysMemSlicePitch: 0,
            };
            let mut pattern = None;
            // SAFETY: `px` vive durante la llamada y tiene el tamaño declarado.
            unsafe { device.CreateTexture2D(&pdesc, Some(&init), Some(&mut pattern)) }?;
            Ok(Self {
                context: setup.context.clone(),
                frames,
                pattern: pattern.ok_or_else(|| anyhow!("patrón nulo"))?,
                width,
                height,
                noise: vec![0u8; 256 * 64 * 4],
                rng,
            })
        }

        /// Dibuja el frame `n` y devuelve su textura.
        fn render(&mut self, n: u64) -> ID3D11Texture2D {
            let (tex, rtv) = self.frames[(n % self.frames.len() as u64) as usize].clone();
            let t = n as f32 / 60.0;
            let color = [
                0.5 + 0.4 * (t * 0.7).sin(),
                0.5 + 0.4 * (t * 1.1).sin(),
                0.5 + 0.4 * (t * 1.3).cos(),
                1.0,
            ];
            // SAFETY: recursos válidos del mismo dispositivo; las cajas están
            // dentro de los límites de origen y destino.
            unsafe {
                self.context.ClearRenderTargetView(&rtv, &color);
                let maxx = self.width - PATTERN;
                let maxy = self.height - PATTERN;
                for k in 0..4u64 {
                    let sx = 3 + 2 * k;
                    let sy = 2 + k;
                    let px = bounce(n * sx + k * 300, maxx);
                    let py = bounce(n * sy + k * 170, maxy);
                    self.context
                        .CopySubresourceRegion(&tex, 0, px, py, 0, &self.pattern, 0, None);
                }
                for b in self.noise.chunks_exact_mut(8) {
                    self.rng = self
                        .rng
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    b.copy_from_slice(&self.rng.to_le_bytes());
                }
                let bx = D3D11_BOX {
                    left: 32,
                    top: 32,
                    front: 0,
                    right: 32 + 256,
                    bottom: 32 + 64,
                    back: 1,
                };
                self.context.UpdateSubresource(
                    &tex,
                    0,
                    Some(&bx),
                    self.noise.as_ptr() as *const _,
                    256 * 4,
                    0,
                );
            }
            tex
        }
    }

    fn bounce(v: u64, max: u32) -> u32 {
        let m = max as u64;
        if m == 0 {
            return 0;
        }
        let p = v % (2 * m);
        (if p < m { p } else { 2 * m - p }) as u32
    }

    // ----------------------------------------------- tiempo GPU de la conversión

    fn get_data_blocking(
        ctx: &ID3D11DeviceContext,
        q: &ID3D11Query,
        out: *mut core::ffi::c_void,
        size: u32,
    ) {
        loop {
            // SAFETY: llamada directa a la vtable para distinguir S_FALSE (no
            // listo) de S_OK; `out` apunta a `size` bytes válidos.
            let hr = unsafe {
                (Interface::vtable(ctx).GetData)(Interface::as_raw(ctx), q.as_raw(), out, size, 0)
            };
            if hr.0 == 0 || hr.is_err() {
                return;
            }
            std::thread::yield_now();
        }
    }

    /// Mide la conversión BGRA→NV12 aislada. Las consultas de *timestamp* del
    /// contexto 3D no acotan el trabajo del Video Processor en Intel (corre
    /// en otro motor: daban 0,02 ms a 4K), así que se mide en CPU el tiempo
    /// hasta que una consulta `D3D11_QUERY_EVENT` posterior se completa, y se
    /// resta el mismo ciclo sin conversión (coste del sondeo y del *flush*).
    /// Devuelve (p50, p95, máx, base p50) en ms.
    fn measure_conversion(
        setup: &D3D11Setup,
        w: u32,
        h: u32,
        iters: usize,
    ) -> anyhow::Result<(f64, f64, f64, f64)> {
        let mut synth = Synth::new(setup, w, h)?;
        let mut conv = Converter::new(&setup.device, (w, h), (w, h), (60, 1), 4)?;
        let ctx = setup.context.clone();
        let mut q = None;
        let d = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        // SAFETY: descriptor válido.
        unsafe { setup.device.CreateQuery(&d, Some(&mut q)) }?;
        let q = q.ok_or_else(|| anyhow!("consulta nula"))?;
        let mut done = 0u32;
        let mut wait = |ctx: &ID3D11DeviceContext| {
            // SAFETY: consulta válida del mismo dispositivo.
            unsafe { ctx.End(&q) };
            ctx_flush(ctx);
            get_data_blocking(ctx, &q, &mut done as *mut _ as *mut _, 4);
        };
        let mut conv_ms = Vec::new();
        let mut base_ms = Vec::new();
        for i in 0..iters {
            let tex = synth.render(i as u64);
            wait(&ctx); // el frame sintético ya está pintado
            let t = Instant::now();
            wait(&ctx);
            let base = t.elapsed();
            let t = Instant::now();
            conv.convert(&tex, 0)?;
            wait(&ctx);
            let c = t.elapsed();
            if i >= 10 {
                base_ms.push(base.as_secs_f64() * 1e3);
                conv_ms.push(c.as_secs_f64() * 1e3);
            }
        }
        conv_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        base_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Ok((
            pct(&conv_ms, 0.5),
            pct(&conv_ms, 0.95),
            *conv_ms.last().unwrap_or(&f64::NAN),
            pct(&base_ms, 0.5),
        ))
    }

    fn ctx_flush(ctx: &ID3D11DeviceContext) {
        // SAFETY: contexto válido.
        unsafe { ctx.Flush() };
    }

    // ------------------------------------------------------------- validación

    #[derive(Debug, Default)]
    struct StreamCheck {
        units: usize,
        units_without_start_code: usize,
        nal_counts: [usize; 32],
        idr_units: usize,
        idr_units_with_sps_pps: usize,
        b_slices: usize,
        p_slices: usize,
        i_slices: usize,
        slices_per_frame_max: usize,
        sps: Option<h264::Sps>,
        cabac: Option<bool>,
        forbidden_bits: usize,
    }

    fn check_units(units: &[(Vec<u8>, bool)]) -> StreamCheck {
        let mut c = StreamCheck::default();
        for (data, _) in units {
            c.units += 1;
            if !annexb::starts_with_start_code(data) {
                c.units_without_start_code += 1;
            }
            let nals = annexb::split_nals(data);
            let mut slices = 0;
            let (mut has_sps, mut has_pps, mut has_idr) = (false, false, false);
            for n in &nals {
                let t = n.nal_type();
                c.nal_counts[t as usize] += 1;
                if n.forbidden_bit() {
                    c.forbidden_bits += 1;
                }
                match t {
                    nal_type::SPS => {
                        has_sps = true;
                        if c.sps.is_none() {
                            c.sps = h264::parse_sps(n.data).ok();
                        }
                    }
                    nal_type::PPS => {
                        has_pps = true;
                        if c.cabac.is_none() {
                            c.cabac = h264::parse_pps_start(n.data).ok().map(|p| p.cabac);
                        }
                    }
                    nal_type::IDR_SLICE | nal_type::NON_IDR_SLICE => {
                        has_idr |= t == nal_type::IDR_SLICE;
                        slices += 1;
                        match h264::parse_slice_start(n.data).map(|s| s.slice_type) {
                            Ok(SliceType::B) => c.b_slices += 1,
                            Ok(SliceType::P) => c.p_slices += 1,
                            Ok(SliceType::I) => c.i_slices += 1,
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            c.slices_per_frame_max = c.slices_per_frame_max.max(slices);
            if has_idr {
                c.idr_units += 1;
                if has_sps && has_pps {
                    c.idr_units_with_sps_pps += 1;
                }
            }
        }
        c
    }

    fn print_check(c: &StreamCheck, w: u32, h: u32) {
        let nz: Vec<String> = c
            .nal_counts
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(t, n)| format!("tipo{t}={n}"))
            .collect();
        println!(
            "    validación: unidades={} sin_start_code={} NAL[{}] forbidden_bit={}",
            c.units,
            c.units_without_start_code,
            nz.join(" "),
            c.forbidden_bits
        );
        println!(
            "    slices: I={} P={} B={} máx_slices/frame={} | IDR={} (con SPS+PPS: {})",
            c.i_slices,
            c.p_slices,
            c.b_slices,
            c.slices_per_frame_max,
            c.idr_units,
            c.idr_units_with_sps_pps
        );
        if let Some(s) = &c.sps {
            let ok = s.width == w && s.height == h;
            println!(
                "    SPS: profile_idc={} level_idc={} {}x{} (codificado {}x{}) refs={} poc_type={} CABAC={:?} -> dims {}",
                s.profile_idc, s.level_idc, s.width, s.height, s.coded_width, s.coded_height,
                s.max_num_ref_frames, s.pic_order_cnt_type, c.cabac, if ok { "OK" } else { "NO COINCIDEN" }
            );
            match &s.vui {
                Some(v) => println!(
                    "    VUI: full_range={:?} colour={:?} timing={:?} nal_hrd={:?} low_delay={:?} max_num_reorder_frames={:?} max_dec_frame_buffering={:?}",
                    v.video_full_range, v.colour_description, v.timing, v.nal_hrd, v.low_delay_hrd, v.max_num_reorder_frames, v.max_dec_frame_buffering
                ),
                None => println!("    VUI: ausente"),
            }
        } else {
            println!("    SPS: no encontrado o no analizable");
        }
    }

    // ------------------------------------------------------------- una pasada

    struct RunResult {
        label: String,
        frames_in: u64,
        units: Vec<(Vec<u8>, bool)>,
        latencies: Vec<Duration>,
        hw_times: Vec<Duration>,
        call_times: Vec<Duration>,
        idr_requests: Vec<(u64, bool, Duration)>,
        wall: Duration,
        late: u64,
        prepended: u64,
        unmatched: usize,
    }

    #[allow(clippy::too_many_arguments)]
    fn run_once(
        setup: &D3D11Setup,
        w: u32,
        h: u32,
        kbps: u32,
        frames: u64,
        paced: bool,
        idr_every: Option<u64>,
        bitrate_switch: Option<(u64, u32)>,
    ) -> anyhow::Result<(RunResult, MfH264Encoder)> {
        run_cfg(
            setup,
            EncoderConfig::desktop(w, h, 60, kbps),
            frames,
            paced,
            idr_every,
            bitrate_switch,
        )
    }

    fn run_cfg(
        setup: &D3D11Setup,
        cfg: EncoderConfig,
        frames: u64,
        paced: bool,
        idr_every: Option<u64>,
        bitrate_switch: Option<(u64, u32)>,
    ) -> anyhow::Result<(RunResult, MfH264Encoder)> {
        let (w, h, kbps) = (cfg.width, cfg.height, cfg.bitrate_kbps);
        let mut enc = MfH264Encoder::new(&setup.device, cfg.clone())
            .with_context(|| format!("crear codificador {w}x{h} @ {kbps} kbps"))?;
        let mut synth = Synth::new(setup, w, h)?;
        let pacer = Pacer::new()?;
        let frame_dur = Duration::from_micros(cfg.frame_duration_us());
        let mut r = RunResult {
            label: format!(
                "{w}x{h} @ {} Mbps{}",
                kbps / 1000,
                if paced { "" } else { " (sin ritmo)" }
            ),
            frames_in: 0,
            units: Vec::new(),
            latencies: Vec::new(),
            hw_times: Vec::new(),
            call_times: Vec::new(),
            idr_requests: Vec::new(),
            wall: Duration::ZERO,
            late: 0,
            prepended: 0,
            unmatched: 0,
        };
        let start = Instant::now();
        for n in 0..frames {
            if paced {
                pacer.sleep_until(start + frame_dur * n as u32);
            }
            let tex = synth.render(n);
            let pts_us = n * cfg.frame_duration_us();
            let force_idr = idr_every.is_some_and(|k| n > 0 && n % k == 0);
            let bitrate_kbps = bitrate_switch.and_then(|(at, k)| (n == at).then_some(k));
            let frame = InputFrame {
                texture: tex,
                subresource: 0,
                pts_us,
            };
            let t = Instant::now();
            let units = enc.encode(
                &frame,
                EncodeControl {
                    force_idr,
                    bitrate_kbps,
                },
            )?;
            r.call_times.push(t.elapsed());
            r.frames_in += 1;
            let mut matched = false;
            for u in units {
                r.hw_times.push(u.hw_time);
                if u.pts_us == pts_us {
                    matched = true;
                    r.latencies.push(u.encode_time);
                    if force_idr {
                        r.idr_requests.push((n, u.keyframe, u.encode_time));
                    }
                } else {
                    r.latencies.push(u.encode_time);
                }
                r.units.push((u.data, u.keyframe));
            }
            if !matched {
                r.unmatched += 1;
                if force_idr {
                    r.idr_requests.push((n, false, Duration::ZERO));
                }
            }
        }
        for u in enc.flush()? {
            r.units.push((u.data, u.keyframe));
        }
        r.wall = start.elapsed();
        r.late = enc.stats().late_outputs;
        r.prepended = enc.stats().parameter_sets_prepended;
        Ok((r, enc))
    }

    fn report_run(r: &RunResult, w: u32, h: u32) {
        let (p50, p95, p99, max) = stats_ms(&r.latencies);
        let (c50, c95, _, cmax) = stats_ms(&r.call_times);
        let total: usize = r.units.iter().map(|(d, _)| d.len()).sum();
        let secs = r.frames_in as f64 / 60.0;
        let kbps = total as f64 * 8.0 / secs / 1000.0;
        let idr: Vec<usize> = r
            .units
            .iter()
            .filter(|(_, k)| *k)
            .map(|(d, _)| d.len())
            .collect();
        let p: Vec<usize> = r
            .units
            .iter()
            .filter(|(_, k)| !*k)
            .map(|(d, _)| d.len())
            .collect();
        let avg = |v: &[usize]| {
            if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<usize>() as f64 / v.len() as f64
            }
        };
        let pmax = p.iter().copied().max().unwrap_or(0);
        println!("  == {} ==", r.label);
        println!(
            "    frames in/out={}/{} en {:.2} s -> {:.1} fps logrados | sin salida en la misma llamada={} | tardías={} | SPS/PPS antepuestos={}",
            r.frames_in, r.units.len(), r.wall.as_secs_f64(), r.units.len() as f64 / r.wall.as_secs_f64(), r.unmatched, r.late, r.prepended
        );
        println!(
            "    latencia envío→salida (ms): p50={p50:.2} p95={p95:.2} p99={p99:.2} máx={max:.2}"
        );
        let (h50, h95, h99, hmax) = stats_ms(&r.hw_times);
        println!("    solo MFT ProcessInput→salida (ms): p50={h50:.2} p95={h95:.2} p99={h99:.2} máx={hmax:.2}");
        println!("    duración de encode() completa (ms): p50={c50:.2} p95={c95:.2} máx={cmax:.2}");
        println!(
            "    tasa de salida={:.0} kbps | IDR: n={} media={:.0} B | P: n={} media={:.0} B máx={} B | IDR/P={:.1}x",
            kbps, idr.len(), avg(&idr), p.len(), avg(&p), pmax, avg(&idr) / avg(&p).max(1.0)
        );
        if !r.idr_requests.is_empty() {
            let ok = r.idr_requests.iter().filter(|(_, k, _)| *k).count();
            let lat: Vec<String> = r
                .idr_requests
                .iter()
                .map(|(n, k, d)| {
                    format!(
                        "#{n}:{}{:.2}ms",
                        if *k { "IDR " } else { "NO-IDR " },
                        d.as_secs_f64() * 1e3
                    )
                })
                .collect();
            println!(
                "    IDR bajo demanda: {}/{} salieron como IDR en el mismo frame [{}]",
                ok,
                r.idr_requests.len(),
                lat.join(", ")
            );
        }
        print_check(&check_units(&r.units), w, h);
    }

    fn write_stream(dir: &Path, name: &str, units: &[(Vec<u8>, bool)]) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let p = dir.join(name);
        let mut all = Vec::new();
        for (d, _) in units {
            all.extend_from_slice(d);
        }
        std::fs::write(&p, &all)?;
        Ok(p)
    }

    fn print_props(props: &[PropertyReport]) {
        for p in props {
            let o = match &p.outcome {
                PropertyOutcome::Accepted { readback } => {
                    format!("ACEPTADA (lectura: {})", readback.as_deref().unwrap_or("-"))
                }
                PropertyOutcome::NotSupported => "NO SOPORTADA (IsSupported=false)".into(),
                PropertyOutcome::Rejected { hresult } => {
                    format!("RECHAZADA hr=0x{:08X}", *hresult as u32)
                }
                PropertyOutcome::Queried { value } => {
                    format!("consulta: {}", value.as_deref().unwrap_or("-"))
                }
            };
            println!(
                "    {:<48} [{}] pedido={:<22} -> {}",
                p.name, p.stage, p.requested, o
            );
        }
    }

    /// Experimentos con propiedades opcionales en un codificador desechable.
    fn experiments(setup: &D3D11Setup) -> anyhow::Result<()> {
        println!("\n## Experimentos con propiedades opcionales (1920x1080 @ 20 Mbps, 120 frames)");
        type Exp = (&'static str, windows::core::GUID, VARIANT);
        let exps: Vec<Exp> = vec![
            (
                "CODECAPI_AVEncVideoTemporalLayerCount=2",
                CODECAPI_AVEncVideoTemporalLayerCount,
                VARIANT::from(2u32),
            ),
            (
                "CODECAPI_AVEncVideoGradualIntraRefresh=1 (UI4)",
                CODECAPI_AVEncVideoGradualIntraRefresh,
                VARIANT::from(1u32),
            ),
            (
                "CODECAPI_AVEncVideoGradualIntraRefresh=1 (UI8)",
                CODECAPI_AVEncVideoGradualIntraRefresh,
                VARIANT::from(1u64),
            ),
            (
                "CODECAPI_AVEncVideoLTRBufferControl=0x10002",
                CODECAPI_AVEncVideoLTRBufferControl,
                VARIANT::from(0x0001_0002u32),
            ),
            (
                "CODECAPI_AVEncVideoMaxNumRefFrame=1",
                CODECAPI_AVEncVideoMaxNumRefFrame,
                VARIANT::from(1u32),
            ),
            (
                "CODECAPI_AVEncSliceControlMode=2 (filas MB)",
                CODECAPI_AVEncSliceControlMode,
                VARIANT::from(2u32),
            ),
            (
                "CODECAPI_AVEncSliceControlSize=17 filas",
                CODECAPI_AVEncSliceControlSize,
                VARIANT::from(17u32),
            ),
            (
                "CODECAPI_AVEncCommonQualityVsSpeed=0 (velocidad)",
                CODECAPI_AVEncCommonQualityVsSpeed,
                VARIANT::from(0u32),
            ),
            (
                "CODECAPI_AVEncVideoMinQP=20",
                CODECAPI_AVEncVideoMinQP,
                VARIANT::from(20u32),
            ),
            (
                "CODECAPI_AVEncVideoMaxQP=45",
                CODECAPI_AVEncVideoMaxQP,
                VARIANT::from(45u32),
            ),
            (
                "CODECAPI_AVEncCommonRateControlMode=LowDelayVBR",
                CODECAPI_AVEncCommonRateControlMode,
                VARIANT::from(eAVEncCommonRateControlMode_LowDelayVBR.0 as u32),
            ),
        ];
        for (name, guid, value) in exps {
            let cfg = EncoderConfig::desktop(1920, 1080, 60, 20_000);
            let mut enc = match MfH264Encoder::new_with_properties(
                &setup.device,
                cfg.clone(),
                &[(name, guid, value)],
            ) {
                Ok(e) => e,
                Err(e) => {
                    println!("    {name}: no se pudo crear el codificador: {e}");
                    continue;
                }
            };
            let api = enc
                .codec_api()
                .cloned()
                .ok_or_else(|| anyhow!("sin ICodecAPI"))?;
            let rep = enc
                .caps()
                .properties
                .iter()
                .find(|p| p.name == name)
                .cloned()
                .ok_or_else(|| anyhow!("sin informe de {name}"))?;
            let q = query_codec_property(&api, name, &guid);
            let mut synth = Synth::new(setup, 1920, 1080)?;
            let mut units = Vec::new();
            let mut lats = Vec::new();
            let mut err = None;
            for n in 0..120u64 {
                let f = InputFrame {
                    texture: synth.render(n),
                    subresource: 0,
                    pts_us: n * cfg.frame_duration_us(),
                };
                match enc.encode(&f, EncodeControl::default()) {
                    Ok(us) => {
                        for u in us {
                            lats.push(u.encode_time);
                            units.push((u.data, u.keyframe));
                        }
                    }
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            }
            let c = check_units(&units);
            let (p50, p95, _, _) = stats_ms(&lats);
            let bytes: usize = units.iter().map(|(d, _)| d.len()).sum();
            let o = match &rep.outcome {
                PropertyOutcome::Accepted { readback } => {
                    format!("ACEPTADA (lectura {})", readback.as_deref().unwrap_or("-"))
                }
                PropertyOutcome::NotSupported => "NO SOPORTADA".into(),
                PropertyOutcome::Rejected { hresult } => {
                    format!("RECHAZADA 0x{:08X}", *hresult as u32)
                }
                PropertyOutcome::Queried { .. } => "-".into(),
            };
            let qs = match q.outcome {
                PropertyOutcome::Queried { value } => value.unwrap_or_default(),
                _ => String::new(),
            };
            println!(
                "    {name}: {o} | {qs} | frames={} I={} P={} B={} IDR={} máx_slices/frame={} | {:.0} kbps | p50={p50:.2} p95={p95:.2} ms{}",
                units.len(), c.i_slices, c.p_slices, c.b_slices, c.idr_units, c.slices_per_frame_max,
                bytes as f64 * 8.0 / 2.0 / 1000.0,
                err.map(|e| format!(" | ERROR: {e}")).unwrap_or_default()
            );
        }
        Ok(())
    }

    pub fn run() -> anyhow::Result<()> {
        let args: Vec<String> = std::env::args().collect();
        let quick = args.iter().any(|a| a == "--quick");
        let out_dir = args
            .iter()
            .position(|a| a == "--out")
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_OUT));
        let frames: u64 = if quick { 120 } else { 600 };
        let idr_every: u64 = if quick { 40 } else { 120 };
        let only1080 = args.iter().any(|a| a == "--only1080");
        let variants = args.iter().any(|a| a == "--variants");
        let setup = create_d3d11_device(Some(0x8086))?;
        println!("# PoC-3 encode_bench");
        println!(
            "adaptador: {} (vendor 0x{:04X}, LUID 0x{:016X})",
            setup.adapter_name, setup.vendor_id, setup.luid
        );
        println!("\n## MFT H.264 hardware enumerados");
        for (i, _) in enumerate_hw_h264_encoders()? {
            println!(
                "    {} | vendor={:?} | LUID={:?}",
                i.friendly_name,
                i.vendor_id,
                i.adapter_luid.map(|l| format!("0x{l:016X}"))
            );
        }

        if variants {
            println!("\n## Variantes de configuración (1080p @ 20 Mbps, 240 frames con ritmo)");
            for (timer, scen, ll, vbv) in [
                (false, true, true, Some(1.0)),
                (true, true, true, Some(1.0)),
                (true, false, true, Some(1.0)),
                (true, true, false, Some(1.0)),
                (true, true, true, None),
                (true, false, false, None),
            ] {
                let mut cfg = EncoderConfig::desktop(1920, 1080, 60, 20_000);
                cfg.raise_timer_resolution = timer;
                cfg.display_remoting_scenario = scen;
                cfg.low_latency = ll;
                cfg.vbv_frames = vbv;
                match run_cfg(&setup, cfg, 240, true, None, None) {
                    Ok((r, _)) => {
                        let (p50, p95, _, max) = stats_ms(&r.latencies);
                        let (h50, h95, _, _) = stats_ms(&r.hw_times);
                        println!(
                            "    timer1ms={timer} DisplayRemoting={scen} LowLatency={ll} VBV={vbv:?}: {:.1} fps | latencia p50={p50:.2} p95={p95:.2} máx={max:.2} | solo MFT p50={h50:.2} p95={h95:.2} | sin salida inmediata={}",
                            r.units.len() as f64 / r.wall.as_secs_f64(), r.unmatched
                        );
                    }
                    Err(e) => println!(
                        "    timer1ms={timer} DisplayRemoting={scen} LowLatency={ll}: ERROR {e:#}"
                    ),
                }
            }
            return Ok(());
        }

        println!("\n## Conversión BGRA→NV12 (Video Processor D3D11; CPU hasta completar D3D11_QUERY_EVENT, 300 iteraciones)");
        for (w, h) in [(1920u32, 1080u32), (2560, 1440), (3840, 2160)] {
            match measure_conversion(&setup, w, h, 300) {
                Ok((p50, p95, max, base)) => println!("    {w}x{h}: p50={p50:.3} ms p95={p95:.3} ms máx={max:.3} ms (ciclo vacío p50={base:.3} ms)"),
                Err(e) => println!("    {w}x{h}: ERROR {e:#}"),
            }
        }

        // Pasada principal con informe de propiedades.
        let plan: Vec<(u32, u32, Vec<u32>)> = if only1080 {
            vec![(1920, 1080, vec![20_000])]
        } else {
            vec![
                (1920, 1080, vec![10_000, 20_000, 40_000]),
                (2560, 1440, vec![20_000, 40_000]),
                (3840, 2160, vec![40_000, 60_000]),
            ]
        };
        let mut props_printed = false;
        for (w, h, rates) in plan {
            println!("\n## {w}x{h} @ 60 fps, {frames} frames con ritmo real, IDR forzado cada {idr_every} frames");
            for kbps in rates {
                match run_once(&setup, w, h, kbps, frames, true, Some(idr_every), None) {
                    Ok((r, enc)) => {
                        if !props_printed {
                            let c = enc.caps();
                            println!(
                                "\n## Codificador: {} (vendor {:?})",
                                c.info.friendly_name, c.info.vendor_id
                            );
                            println!(
                                "    async={} d3d11_aware={} provides_samples={} MF_MT_MPEG_SEQUENCE_HEADER={} ({} B) NV12 bind=0x{:X}",
                                c.async_mft, c.d3d11_aware, c.provides_output_samples, c.sequence_header_in_type,
                                enc.sequence_header().len(), enc.nv12_bind_flags()
                            );
                            print_props(&c.properties);
                            props_printed = true;
                        }
                        report_run(&r, w, h);
                        let name = format!("bench_{}p_{}mbps.h264", h, kbps / 1000);
                        let p = write_stream(&out_dir, &name, &r.units)?;
                        println!("    escrito: {}", p.display());
                        if w == 1920 && kbps == 20_000 {
                            let p = write_stream(&out_dir, "bench_1080p.h264", &r.units)?;
                            println!("    escrito: {}", p.display());
                        }
                    }
                    Err(e) => println!("  {w}x{h} @ {kbps} kbps: ERROR {e:#}"),
                }
            }
        }

        if only1080 {
            return Ok(());
        }
        println!("\n## Rendimiento máximo sin ritmo (300 frames, 20/40 Mbps)");
        for (w, h, kbps) in [
            (1920u32, 1080u32, 20_000u32),
            (2560, 1440, 40_000),
            (3840, 2160, 40_000),
        ] {
            match run_once(&setup, w, h, kbps, 300, false, None, None) {
                Ok((r, _)) => {
                    let (p50, p95, _, max) = stats_ms(&r.latencies);
                    let (h50, h95, _, _) = stats_ms(&r.hw_times);
                    println!(
                        "    {w}x{h}: {:.1} fps | latencia p50={p50:.2} p95={p95:.2} máx={max:.2} ms | solo MFT p50={h50:.2} p95={h95:.2} | sin salida inmediata={}",
                        r.units.len() as f64 / r.wall.as_secs_f64(), r.unmatched
                    );
                }
                Err(e) => println!("    {w}x{h}: ERROR {e:#}"),
            }
        }

        println!(
            "\n## Cambio de tasa en caliente (1080p: 20 Mbps → 5 Mbps en el frame 300, 600 frames)"
        );
        match run_once(
            &setup,
            1920,
            1080,
            20_000,
            600,
            true,
            None,
            Some((300, 5_000)),
        ) {
            Ok((r, enc)) => {
                let before: usize = r.units[60..300.min(r.units.len())]
                    .iter()
                    .map(|(d, _)| d.len())
                    .sum();
                let after: usize = r.units[360.min(r.units.len())..]
                    .iter()
                    .map(|(d, _)| d.len())
                    .sum();
                let n_after = r.units.len().saturating_sub(360).max(1);
                println!(
                    "    antes: {:.0} kbps | después: {:.0} kbps | tasa actual en el codificador: {} kbps",
                    before as f64 * 8.0 * 60.0 / 240.0 / 1000.0,
                    after as f64 * 8.0 * 60.0 / n_after as f64 / 1000.0,
                    enc.bitrate_kbps()
                );
                if let Some(p) = enc
                    .caps()
                    .properties
                    .iter()
                    .find(|p| p.stage == "en streaming")
                {
                    print_props(std::slice::from_ref(p));
                }
            }
            Err(e) => println!("    ERROR {e:#}"),
        }

        experiments(&setup)?;
        Ok(())
    }
}
