//! Métricas del emisor (las que pide el contrato al terminar).

use cs_capture::stats::Samples;
use cs_protocol::control::ReceiverStats;
use std::fmt;
use std::time::Duration;

/// Contadores del hilo de captura + codificación.
#[derive(Debug, Clone, Default)]
pub struct WorkerStats {
    /// Frames con imagen nueva entregados por la captura.
    pub captured: u64,
    /// Unidades H.264 producidas por el codificador.
    pub encoded: u64,
    /// Frames descartados **antes** de codificar porque el envío no daba abasto.
    pub dropped_backpressure: u64,
    /// Unidades ya codificadas que no cupieron en el canal (rompen la cadena
    /// de referencias: se fuerza un IDR después). Debería ser 0.
    pub dropped_after_encode: u64,
    /// Re-codificaciones del último frame con el escritorio estático
    /// (refresco periódico o IDR pedido sin frames nuevos).
    pub repeats: u64,
    /// IDR pedidos al codificador (`force_idr`).
    pub forced_idr: u64,
    /// Unidades marcadas como IDR a la salida del codificador.
    pub idr_out: u64,
    /// Errores del codificador (el frame se pierde).
    pub encoder_errors: u64,
    /// Codificadores creados (1 + recreaciones por cambio de tamaño, de
    /// dispositivo o tras errores).
    pub encoders_created: u64,
    /// `ACCESS_LOST` y recreaciones de la captura.
    pub access_lost: u64,
    pub capture_reinits: u64,
    /// Frames con `AccumulatedFrames > 1` (DWM presentó más de una vez entre
    /// dos adquisiciones: frames del compositor que no vimos).
    pub accumulated_gt1: u64,
    /// Coste de la captura en CPU: adquisición → copia encolada (µs).
    pub capture_us: Samples,
    /// Presentación de DWM → adquisición (µs).
    pub present_to_acquire_us: Samples,
    /// `encode()`: conversión NV12 + MFT hasta tener los bytes en CPU (µs).
    pub encode_us: Samples,
    /// Solo el MFT (µs).
    pub hw_us: Samples,
    /// Tamaño medio de IDR y de P (bytes).
    pub idr_bytes: u64,
    pub p_bytes: u64,
    /// Tamaño del último frame capturado.
    pub last_size: Option<(u32, u32)>,
    /// Error fatal que terminó el hilo, si lo hubo.
    pub fatal: Option<String>,
}

/// Contadores de la tarea de envío.
#[derive(Debug, Clone, Default)]
pub struct SendStats {
    pub frames_sent: u64,
    pub keyframes_sent: u64,
    /// Bytes H.264 enviados (sin cabeceras ni FEC).
    pub payload_bytes: u64,
    /// Bytes de datagramas (con cabeceras de fragmento y paridad FEC).
    pub wire_bytes: u64,
    pub datagrams_sent: u64,
    pub datagrams_failed: u64,
    /// Datagramas descartados por el inyector de pérdidas (`--sim-loss`).
    pub sim_dropped_datagrams: u64,
    /// Ráfagas de pérdidas simuladas y la más larga.
    pub sim_bursts: u64,
    pub sim_longest_burst: u64,
    /// Captura (pts QPC) → último datagrama entregado a QUIC (µs).
    pub capture_to_sent_us: Samples,
    /// Mayor retraso de un datagrama respecto a su plan de *pacing*.
    pub max_pacing_late: Duration,
}

/// Contadores del canal de control.
#[derive(Debug, Clone, Default)]
pub struct ControlStats {
    pub keyframe_requests: u64,
    pub keyframes_forced: u64,
    /// Peticiones ignoradas por llegar dentro de ~1 RTT del último IDR forzado.
    pub keyframe_requests_ignored: u64,
    pub cursor_positions: u64,
    pub cursor_shapes: u64,
    pub receiver_stats: u64,
    pub last_receiver_stats: Option<ReceiverStats>,
    /// Cambios de paridad del FEC adaptativo y rango alcanzado.
    pub fec_changes: u64,
    pub fec_initial: f32,
    pub fec_final: f32,
    pub fec_min: f32,
    pub fec_max: f32,
    pub keyframe_fec: f32,
    pub fec_interleaved: bool,
}

/// Informe final del emisor.
#[derive(Debug, Clone, Default)]
pub struct SenderReport {
    pub duration: Duration,
    pub end_reason: String,
    pub worker: WorkerStats,
    pub send: SendStats,
    pub control: ControlStats,
}

fn ms(s: &mut Samples) -> String {
    match (s.percentile(50.0), s.percentile(95.0), s.max()) {
        (Some(a), Some(b), Some(c)) => format!(
            "p50 {:.2} / p95 {:.2} / máx {:.2} ms (n={})",
            a / 1000.0,
            b / 1000.0,
            c / 1000.0,
            s.len()
        ),
        _ => "sin muestras".into(),
    }
}

impl fmt::Display for SenderReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut r = self.clone();
        let secs = r.duration.as_secs_f64().max(1e-6);
        let w = &mut r.worker;
        let s = &mut r.send;
        let c = &r.control;
        writeln!(
            f,
            "── Emisor: resumen ({:.1} s, fin: {}) ──",
            secs, r.end_reason
        )?;
        if let Some((wd, ht)) = w.last_size {
            writeln!(f, "Tamaño capturado:        {wd}x{ht}")?;
        }
        writeln!(
            f,
            "fps capturados / codificados / enviados: {:.1} / {:.1} / {:.1}",
            w.captured as f64 / secs,
            w.encoded as f64 / secs,
            s.frames_sent as f64 / secs
        )?;
        writeln!(
            f,
            "Frames: capturados {} · codificados {} · enviados {} · descartados antes de codificar {} · tras codificar {} · repeticiones (estático) {}",
            w.captured, w.encoded, s.frames_sent, w.dropped_backpressure, w.dropped_after_encode, w.repeats
        )?;
        writeln!(f, "Captura (CPU, acquire→copia): {}", ms(&mut w.capture_us))?;
        writeln!(
            f,
            "DWM present→acquire:          {}",
            ms(&mut w.present_to_acquire_us)
        )?;
        writeln!(f, "Codificación (NV12+MFT):      {}", ms(&mut w.encode_us))?;
        writeln!(f, "  solo MFT:                   {}", ms(&mut w.hw_us))?;
        writeln!(
            f,
            "Captura→enviado (QPC):        {}",
            ms(&mut s.capture_to_sent_us)
        )?;
        writeln!(
            f,
            "Bitrate real: {:.2} Mbps H.264 · {:.2} Mbps en datagramas (con FEC y cabeceras)",
            s.payload_bytes as f64 * 8.0 / secs / 1e6,
            s.wire_bytes as f64 * 8.0 / secs / 1e6
        )?;
        let avg = |b: u64, n: u64| {
            if n == 0 {
                0.0
            } else {
                b as f64 / n as f64 / 1024.0
            }
        };
        let p_frames = w.encoded.saturating_sub(w.idr_out);
        writeln!(
            f,
            "IDR: {} a la salida (medio {:.1} KiB; P medio {:.1} KiB) · forzados {} · peticiones del receptor {} (atendidas {}, ignoradas <1 RTT {})",
            w.idr_out,
            avg(w.idr_bytes, w.idr_out),
            avg(w.p_bytes, p_frames),
            w.forced_idr,
            c.keyframe_requests,
            c.keyframes_forced,
            c.keyframe_requests_ignored
        )?;
        writeln!(
            f,
            "FEC: P inicial {:.2} → final {:.2} (rango [{:.2}, {:.2}], {} cambios) · IDR {:.2} · entrelazado {}",
            c.fec_initial,
            c.fec_final,
            c.fec_min,
            c.fec_max,
            c.fec_changes,
            c.keyframe_fec,
            c.fec_interleaved
        )?;
        writeln!(
            f,
            "Datagramas: {} enviados, {} fallidos{} · retraso máx. de pacing {:.2} ms",
            s.datagrams_sent,
            s.datagrams_failed,
            if s.sim_dropped_datagrams > 0 {
                format!(
                    ", {} descartados por --sim-loss ({} ráfagas, máx. {})",
                    s.sim_dropped_datagrams, s.sim_bursts, s.sim_longest_burst
                )
            } else {
                String::new()
            },
            s.max_pacing_late.as_secs_f64() * 1000.0
        )?;
        writeln!(
            f,
            "Captura: ACCESS_LOST {} · recreaciones {} · frames con AccumulatedFrames>1 {} · errores del codificador {} · codificadores creados {}",
            w.access_lost, w.capture_reinits, w.accumulated_gt1, w.encoder_errors, w.encoders_created
        )?;
        writeln!(
            f,
            "Cursor: {} posiciones y {} formas enviadas · ReceiverStats recibidos {}",
            c.cursor_positions, c.cursor_shapes, c.receiver_stats
        )?;
        if let Some(rs) = c.last_receiver_stats {
            writeln!(f, "Último ReceiverStats: {rs:?}")?;
        }
        if let Some(e) = &w.fatal {
            writeln!(f, "ERROR en captura/codificación: {e}")?;
        }
        Ok(())
    }
}
