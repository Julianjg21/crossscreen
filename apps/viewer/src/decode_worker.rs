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
//! Recuperación ante pérdidas ([`LossPolicy`]):
//! - **`DecodeThrough`** (por defecto): tras un `Lost` se siguen entregando
//!   los P-frames al MFT, que oculta las referencias que faltan (artefactos
//!   localizados que se arrastran con el movimiento, en vez de congelar la
//!   imagen), y se pide **un** IDR para sanear. Si el MFT devuelve error o
//!   deja de producir salida, `flush` y se espera al IDR como en el modo
//!   anterior.
//! - **`FreezeOnLoss`** (`--freeze-on-loss`, el comportamiento anterior): no
//!   se entregan P-frames hasta el siguiente IDR y la ventana mantiene el
//!   último frame bueno.
//!
//! En ambos casos, al empezar (sin referencia) no se decodifica nada hasta el
//! primer IDR. Cuántas peticiones salen lo decide la red
//! (`net::KeyframeRequester`); aquí solo se avisa de cada pérdida y de cada
//! IDR decodificado.

use crate::session::{
    DecodeInput, KeyframeSignal, LossPolicy, MailFrame, Mailbox, SessionStats, UserEvent,
};
use cs_decode::annexb::is_idr;
use cs_decode::VideoDecoder;
use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use winit::event_loop::EventLoopProxy;

/// AU seguidas entregadas al MFT sin que salga ningún frame a partir de las
/// cuales se considera que el decodificador se ha atascado (con baja
/// latencia y sin B-frames sale un frame por AU).
const STALL_INPUTS: u32 = 8;
/// Espera máxima de una AU en cola antes de saltar hasta el siguiente IDR.
const MAX_QUEUE_LAG: Duration = Duration::from_millis(250);

/// Lo que necesita el hilo.
pub struct Worker {
    pub device: crate::platform::VideoContext,
    pub codec: cs_core::Codec,
    pub initial_size: Option<(u32, u32)>,
    pub input: Receiver<DecodeInput>,
    pub mailbox: Arc<Mailbox>,
    pub proxy: EventLoopProxy<UserEvent>,
    pub keyframe_tx: UnboundedSender<KeyframeSignal>,
    pub stats: Arc<Mutex<SessionStats>>,
    pub policy: LossPolicy,
}

/// Episodio de pérdida: desde la primera pérdida hasta el IDR que la cura.
struct Episode {
    start: Instant,
    /// `keyframe_requests_sent` al empezar (para contar las del episodio).
    requests_at_start: u64,
    /// Frames decodificados y entregados al buzón durante el episodio.
    frames: u64,
}

/// Cuerpo del hilo. `ready` recibe el resultado de crear el decodificador.
/// Termina cuando la red suelta su extremo del canal (fin de sesión).
pub fn run(w: Worker, ready: std::sync::mpsc::Sender<Result<(), String>>) {
    let mut decoder = match VideoDecoder::new_for(w.codec, &w.device, w.initial_size) {
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
    stats(&mut |s| s.loss_policy = w.policy);
    let signal = |s: KeyframeSignal| {
        let _ = w.keyframe_tx.send(s);
    };
    let requests_now = || {
        w.stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keyframe_requests_sent
    };
    let open_episode = |ep: &mut Option<Episode>, at: Instant| {
        if ep.is_none() {
            *ep = Some(Episode {
                start: at,
                requests_at_start: requests_now(),
                frames: 0,
            });
        }
    };
    // Sin referencia utilizable: los P-frames no se entregan al MFT. Al
    // empezar, tras un error del MFT y (con `FreezeOnLoss`) tras una pérdida.
    let mut need_idr = true;
    // Referencias rotas pero se sigue decodificando (`DecodeThrough`).
    let mut refs_broken = false;
    let mut episode: Option<Episode> = None;
    // AU entregadas seguidas sin salida (detección de atasco del MFT).
    let mut inputs_without_output = 0u32;
    // (pts, frame_seq, recibido) de las AU entregadas al MFT.
    let mut in_flight: VecDeque<(u32, u32, Instant)> = VecDeque::new();
    let mut first_frame_done = false;

    for msg in w.input.iter() {
        match msg {
            DecodeInput::Lost { frame_seq, at } => {
                open_episode(&mut episode, at);
                match w.policy {
                    LossPolicy::FreezeOnLoss => {
                        tracing::debug!("frame {frame_seq} perdido: se espera IDR");
                        need_idr = true;
                    }
                    LossPolicy::DecodeThrough => {
                        tracing::debug!("frame {frame_seq} perdido: se sigue decodificando");
                        if !need_idr {
                            refs_broken = true;
                        }
                    }
                }
                signal(KeyframeSignal::Needed {
                    lost_seq: frame_seq,
                });
            }
            DecodeInput::Frame { frame, received_at } => {
                // La marca de clave viene en la cabecera de vídeo; mirar las
                // NAL solo sirve de respaldo en H.264.
                let key =
                    frame.keyframe || (w.codec == cs_core::Codec::H264 && is_idr(&frame.data));
                if need_idr && !key {
                    stats(&mut |s| s.dropped_waiting_idr += 1);
                    signal(KeyframeSignal::Needed {
                        lost_seq: frame.frame_seq,
                    });
                    continue;
                }
                let queue_wait = received_at.elapsed();
                stats(&mut |s| s.decode_queue_wait.record(queue_wait));
                // Latencia acotada: si el decodificador no da abasto (p. ej.
                // OpenH264 por software en un equipo lento), la cola crece y con
                // ella la latencia sin límite. Se descartan los P-frames
                // atrasados hasta el siguiente IDR, que se pide ya: mejor un
                // salto que un retraso creciente. Los IDR siempre se decodifican.
                if !key && queue_wait > MAX_QUEUE_LAG {
                    tracing::debug!(
                        "frame {} lleva {:.0} ms en cola: se salta hasta el próximo IDR",
                        frame.frame_seq,
                        queue_wait.as_secs_f64() * 1e3
                    );
                    stats(&mut |s| s.dropped_for_lag += 1);
                    need_idr = true;
                    signal(KeyframeSignal::Needed {
                        lost_seq: frame.frame_seq,
                    });
                    continue;
                }
                in_flight.push_back((frame.pts_us, frame.frame_seq, received_at));
                while in_flight.len() > 32 {
                    in_flight.pop_front();
                }
                let failure = match decoder.decode(&frame.data, i64::from(frame.pts_us)) {
                    Ok(out) => {
                        if key {
                            need_idr = false;
                            refs_broken = false;
                            signal(KeyframeSignal::Decoded {
                                frame_seq: frame.frame_seq,
                            });
                            if let Some(ep) = episode.take() {
                                let rec = ep.start.elapsed();
                                let reqs = requests_now().saturating_sub(ep.requests_at_start);
                                tracing::info!(
                                    "recuperado con el IDR {} en {:.1} ms ({} peticiones, {} frames durante el episodio)",
                                    frame.frame_seq,
                                    rec.as_secs_f64() * 1e3,
                                    reqs,
                                    ep.frames
                                );
                                stats(&mut |s| {
                                    s.recovery.record(rec);
                                    s.episode_requests.push(reqs);
                                    s.episode_frames.push(ep.frames);
                                });
                            }
                        }
                        let concealed = refs_broken;
                        let mut produced = 0u32;
                        // Normalmente sale un frame por AU; si saliera más de
                        // uno, todos pasan por el buzón y gana el último.
                        let mut next = out;
                        while let Some(df) = next {
                            produced += 1;
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
                                concealed,
                            });
                            if let Some(ep) = episode.as_mut() {
                                ep.frames += 1;
                            }
                            stats(&mut |s| {
                                s.frames_decoded += 1;
                                if concealed {
                                    s.frames_decoded_concealed += 1;
                                }
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
                        if produced > 0 {
                            inputs_without_output = 0;
                            None
                        } else {
                            inputs_without_output += 1;
                            (inputs_without_output >= STALL_INPUTS).then(|| {
                                stats(&mut |s| s.decode_stalls += 1);
                                format!("{STALL_INPUTS} AU seguidas sin salida")
                            })
                        }
                    }
                    Err(e) => {
                        stats(&mut |s| s.decode_errors += 1);
                        Some(e.to_string())
                    }
                };
                if let Some(why) = failure {
                    // Respaldo: el MFT no tolera la pérdida → vaciarlo y
                    // esperar al IDR (comportamiento de `FreezeOnLoss`).
                    tracing::warn!(
                        "decodificación del frame {}: {why}; flush y se espera IDR",
                        frame.frame_seq
                    );
                    need_idr = true;
                    refs_broken = false;
                    inputs_without_output = 0;
                    open_episode(&mut episode, received_at);
                    in_flight.clear();
                    if let Err(e) = decoder.flush() {
                        tracing::warn!("flush del decodificador: {e}");
                    }
                    signal(KeyframeSignal::Needed {
                        lost_seq: frame.frame_seq,
                    });
                }
            }
        }
    }
    if episode.is_some() {
        stats(&mut |s| s.episode_open_at_end = true);
    }
    // Soltar lo que quede en el buzón antes que el decodificador.
    drop(w.mailbox.take());
    drop(decoder);
}
