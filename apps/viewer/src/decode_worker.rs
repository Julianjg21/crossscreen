//! Hilo de decodificación.
//!
//! **Por qué un hilo propio y no el de la ventana:** el buzón de presentación
//! es de una plaza y «el último gana», y eso solo es correcto con frames ya
//! decodificados: un P-frame comprimido no se puede descartar sin romper la
//! cadena de referencias. Así que todas las AU se decodifican en orden aquí,
//! y el hilo de la ventana, que se bloquea en el objeto *waitable* de la
//! cadena hasta el siguiente *vblank*, recoge solo el más reciente. Además,
//! la decodificación no queda atada al ritmo del panel: si llega una ráfaga
//! (p. ej. tras un hipo de red), se decodifica entera sin esperar al vsync.
//! Ambos hilos usan el **mismo** dispositivo D3D11 (con protección
//! multihilo), así que la superficie del MFT se muestrea sin copias.
//!
//! Recuperación (contrato §Vídeo): tras `Lost`, un error del decodificador o
//! si el primer frame no es IDR, se pide `RequestKeyframe` (la red coalesce a
//! uno cada 100 ms) y no se entregan P-frames al decodificador hasta el
//! siguiente IDR. Mientras, la ventana sigue mostrando el último frame bueno.

use crate::session::{DecodeInput, KeyframeSignal, MailFrame, Mailbox, SessionStats, UserEvent};
use cs_decode::annexb::is_idr;
use cs_decode::VideoDecoder;
use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use winit::event_loop::EventLoopProxy;

/// Lo que necesita el hilo.
pub struct Worker {
    pub device: ID3D11Device,
    pub input: Receiver<DecodeInput>,
    pub mailbox: Arc<Mailbox>,
    pub proxy: EventLoopProxy<UserEvent>,
    pub keyframe_tx: UnboundedSender<KeyframeSignal>,
    pub stats: Arc<Mutex<SessionStats>>,
}

/// Cuerpo del hilo. `ready` recibe el resultado de crear el decodificador.
/// Termina cuando la red suelta su extremo del canal (fin de sesión).
pub fn run(w: Worker, ready: std::sync::mpsc::Sender<Result<(), String>>) {
    let mut decoder = match VideoDecoder::new(&w.device) {
        Ok(d) => {
            let _ = ready.send(Ok(()));
            d
        }
        Err(e) => {
            let _ = ready.send(Err(format!("no se pudo crear el decodificador: {e}")));
            return;
        }
    };
    let started = Instant::now();
    let stats = |f: &mut dyn FnMut(&mut SessionStats)| {
        f(&mut w.stats.lock().unwrap_or_else(|e| e.into_inner()));
    };
    let request_keyframe = || {
        let _ = w.keyframe_tx.send(KeyframeSignal::Needed);
    };
    // Al empezar no hay referencia: hasta el primer IDR no se decodifica nada.
    let mut awaiting_idr = true;
    // Instante de la primera pérdida sin recuperar (para medir la recuperación).
    let mut lost_since: Option<Instant> = None;
    // (pts, frame_seq, recibido) de las AU entregadas al MFT.
    let mut in_flight: VecDeque<(u32, u32, Instant)> = VecDeque::new();
    let mut first_frame_done = false;

    for msg in w.input.iter() {
        match msg {
            DecodeInput::Lost { frame_seq, at } => {
                tracing::debug!("frame {frame_seq} perdido: se espera IDR");
                if !awaiting_idr {
                    awaiting_idr = true;
                    lost_since.get_or_insert(at);
                }
                request_keyframe();
            }
            DecodeInput::Frame { frame, received_at } => {
                let key = frame.keyframe || is_idr(&frame.data);
                if awaiting_idr && !key {
                    stats(&mut |s| s.dropped_waiting_idr += 1);
                    request_keyframe();
                    continue;
                }
                in_flight.push_back((frame.pts_us, frame.frame_seq, received_at));
                while in_flight.len() > 32 {
                    in_flight.pop_front();
                }
                match decoder.decode(&frame.data, i64::from(frame.pts_us)) {
                    Ok(out) => {
                        if key && awaiting_idr {
                            awaiting_idr = false;
                            let _ = w.keyframe_tx.send(KeyframeSignal::Received);
                            if let Some(t) = lost_since.take() {
                                let rec = t.elapsed();
                                tracing::info!(
                                    "recuperado con el IDR {} en {:.1} ms",
                                    frame.frame_seq,
                                    rec.as_secs_f64() * 1e3
                                );
                                stats(&mut |s| s.recovery.record(rec));
                            }
                        }
                        // Normalmente sale un frame por AU; si saliera más de
                        // uno, todos pasan por el buzón y gana el último.
                        let mut next = out;
                        while let Some(df) = next {
                            let decoded_at = Instant::now();
                            let pts = df.pts_us as u32;
                            let (seq, recv) = in_flight
                                .iter()
                                .position(|(p, _, _)| *p == pts)
                                .map(|i| {
                                    let (_, s, r) = in_flight[i];
                                    in_flight.drain(..=i);
                                    (s, r)
                                })
                                .unwrap_or((frame.frame_seq, received_at));
                            let decode_time = df.decode_time;
                            let size = (df.width, df.height);
                            let replaced = w.mailbox.put(MailFrame {
                                frame: df,
                                frame_seq: seq,
                                pts_us: pts,
                                received_at: recv,
                                decoded_at,
                            });
                            stats(&mut |s| {
                                s.frames_decoded += 1;
                                s.decode.record(decode_time);
                                s.last_decode_us = decode_time.as_micros() as u32;
                                s.video_size = size;
                                if replaced {
                                    s.mailbox_replaced += 1;
                                }
                                if !first_frame_done {
                                    s.first_frame = Some(started.elapsed());
                                }
                            });
                            first_frame_done = true;
                            let _ = w.proxy.send_event(UserEvent::FrameReady);
                            next = decoder.next_ready();
                        }
                    }
                    Err(e) => {
                        tracing::warn!("decodificación del frame {}: {e}", frame.frame_seq);
                        stats(&mut |s| s.decode_errors += 1);
                        awaiting_idr = true;
                        lost_since.get_or_insert(received_at);
                        in_flight.clear();
                        if let Err(e) = decoder.flush() {
                            tracing::warn!("flush del decodificador: {e}");
                        }
                        request_keyframe();
                    }
                }
            }
        }
    }
    // Soltar lo que quede en el buzón antes que el decodificador.
    drop(w.mailbox.take());
    drop(decoder);
}
