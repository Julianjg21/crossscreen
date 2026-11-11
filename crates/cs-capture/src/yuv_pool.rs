//! Conversión RGB → I420 repartida entre hilos **fijos** con prioridad alta.
//!
//! [`crate::yuv::packed_to_i420`] crea hilos nuevos en cada frame. rtkit no
//! deja que la prioridad alta pase a los hilos hijos (`SCHED_RESET_ON_FORK`),
//! así que esos hilos competirían a partes iguales con el resto del equipo
//! justo cuando está cargado. Aquí los hilos se crean una vez, cada uno pide
//! su prioridad al empezar y reciben trabajo por canales. Sin `unsafe`: el
//! frame se comparte con `Arc` y cada hilo escribe en sus propios búferes,
//! que el hilo que llama copia a los planos finales (≈1,5 MB a 1366×768,
//! una fracción de milisegundo).

use crate::yuv::{conversion_threads, convert_rows, packed_fits, I420Planes, PackedRgb};
use std::sync::mpsc;
use std::sync::Arc;

/// Búferes de un tramo de filas, reutilizados entre frames.
#[derive(Default)]
struct Chunk {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

struct Task {
    src: Arc<Vec<u8>>,
    stride: usize,
    width: u32,
    height: u32,
    order: PackedRgb,
    first_crow: usize,
    crows: usize,
    out: Chunk,
}

struct Worker {
    tasks: Option<mpsc::Sender<Task>>,
    done: mpsc::Receiver<Chunk>,
    spare: Option<Chunk>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Cerrar el canal termina el bucle del hilo.
        self.tasks.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn worker_loop(tasks: mpsc::Receiver<Task>, done: mpsc::Sender<Chunk>) {
    if let Err(e) = crate::linux_priority::raise_current_thread() {
        tracing::debug!("hilo de conversión con prioridad normal: {e}");
    }
    while let Ok(task) = tasks.recv() {
        let Task {
            src,
            stride,
            width,
            height,
            order,
            first_crow,
            crows,
            mut out,
        } = task;
        let w = width as usize;
        let (cw, ch) = (w.div_ceil(2), (height as usize).div_ceil(2));
        let crows = crows.min(ch.saturating_sub(first_crow));
        let luma_rows = (2 * crows).min(height as usize - 2 * first_crow);
        out.y.resize(luma_rows * w, 0);
        out.u.resize(crows * cw, 0);
        out.v.resize(crows * cw, 0);
        convert_rows(
            &src, stride, width, height, order, first_crow, &mut out.y, &mut out.u, &mut out.v,
        );
        // Soltar el frame antes de avisar: quien llama lo recupera con
        // `Arc::try_unwrap` en cuanto todos han terminado.
        drop(src);
        if done.send(out).is_err() {
            break;
        }
    }
}

/// Conversor con hilos fijos. Se crea en el hilo que codifica.
pub struct ConvertPool {
    workers: Vec<Worker>,
}

impl std::fmt::Debug for ConvertPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConvertPool")
            .field("workers", &self.workers.len())
            .finish()
    }
}

impl ConvertPool {
    /// Pool para imágenes de `pixels` píxeles: tantos hilos como usaría
    /// `packed_to_i420`, contando el que llama (que también convierte).
    pub fn for_pixels(pixels: usize) -> Self {
        let extra = conversion_threads(pixels).saturating_sub(1);
        let mut workers = Vec::with_capacity(extra);
        for i in 0..extra {
            let (task_tx, task_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            match std::thread::Builder::new()
                .name(format!("crossscreen-yuv-{i}"))
                .spawn(move || worker_loop(task_rx, done_tx))
            {
                Ok(join) => workers.push(Worker {
                    tasks: Some(task_tx),
                    done: done_rx,
                    spare: Some(Chunk::default()),
                    join: Some(join),
                }),
                Err(e) => {
                    tracing::warn!("no se pudo crear un hilo de conversión: {e}");
                    break;
                }
            }
        }
        Self { workers }
    }

    /// Hilos que convierten, contando el que llama.
    pub fn threads(&self) -> usize {
        self.workers.len() + 1
    }

    /// Convierte `src` (RGB empaquetado, filas de `stride` bytes) a I420 y
    /// devuelve también `src` para reutilizar su memoria. `None` si `src` no
    /// contiene `height` filas completas.
    pub fn convert(
        &mut self,
        src: Vec<u8>,
        stride: usize,
        width: u32,
        height: u32,
        order: PackedRgb,
    ) -> (Vec<u8>, Option<I420Planes>) {
        if !packed_fits(src.len(), stride, width, height) {
            return (src, None);
        }
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut y = vec![0u8; w * h];
        let mut u = vec![0u8; cw * ch];
        let mut v = vec![0u8; cw * ch];
        let threads = self.threads().min(ch);
        let crows = ch.div_ceil(threads);
        let src = Arc::new(src);

        // Tramo i (i ≥ 1) al hilo i-1; el tramo 0 lo convierte quien llama.
        let mut sent: Vec<(usize, usize)> = Vec::new(); // (hilo, primera fila de croma)
        for i in 1..threads {
            let first_crow = i * crows;
            if first_crow >= ch {
                break;
            }
            let worker = &mut self.workers[i - 1];
            let task = Task {
                src: Arc::clone(&src),
                stride,
                width,
                height,
                order,
                first_crow,
                crows,
                out: worker.spare.take().unwrap_or_default(),
            };
            match worker.tasks.as_ref().map(|t| t.send(task)) {
                Some(Ok(())) => sent.push((i - 1, first_crow)),
                _ => break,
            }
        }
        let own_crows = if sent.is_empty() { ch } else { crows };
        let own_luma = (2 * own_crows).min(h);
        convert_rows(
            &src,
            stride,
            width,
            height,
            order,
            0,
            &mut y[..own_luma * w],
            &mut u[..own_crows * cw],
            &mut v[..own_crows * cw],
        );

        // Recoger los tramos. Si un hilo falló, su tramo se hace aquí.
        let mut next_missing = own_crows;
        let mut failed = false;
        for (wi, first_crow) in sent {
            debug_assert_eq!(first_crow, next_missing);
            let worker = &mut self.workers[wi];
            let rows_c = crows.min(ch - first_crow);
            let rows_y = (2 * rows_c).min(h - 2 * first_crow);
            let (ys, cs) = (2 * first_crow * w, first_crow * cw);
            match worker.done.recv() {
                Ok(chunk) if chunk.y.len() == rows_y * w && chunk.u.len() == rows_c * cw => {
                    y[ys..ys + rows_y * w].copy_from_slice(&chunk.y);
                    u[cs..cs + rows_c * cw].copy_from_slice(&chunk.u);
                    v[cs..cs + rows_c * cw].copy_from_slice(&chunk.v);
                    worker.spare = Some(chunk);
                }
                _ => {
                    failed = true;
                    convert_rows(
                        &src,
                        stride,
                        width,
                        height,
                        order,
                        first_crow,
                        &mut y[ys..ys + rows_y * w],
                        &mut u[cs..cs + rows_c * cw],
                        &mut v[cs..cs + rows_c * cw],
                    );
                }
            }
            next_missing = first_crow + rows_c;
        }
        if next_missing < ch {
            // Algún envío falló: convertir aquí lo que quede.
            failed = true;
            let (ys, cs) = (2 * next_missing * w, next_missing * cw);
            convert_rows(
                &src,
                stride,
                width,
                height,
                order,
                next_missing,
                &mut y[ys..],
                &mut u[cs..],
                &mut v[cs..],
            );
        }
        if failed {
            tracing::warn!("un hilo de conversión falló; se sigue con uno solo");
            self.workers.clear();
        }
        // Todos los hilos soltaron su copia del `Arc` antes de responder; si
        // alguno no lo hizo (falló), se copia (raro, y solo una vez).
        let src = Arc::try_unwrap(src).unwrap_or_else(|shared| shared.as_ref().clone());
        (src, Some(I420Planes { y, u, v }))
    }
}

#[cfg(test)]
mod tests {
    use super::ConvertPool;
    use crate::yuv::{packed_to_i420, PackedRgb};

    #[test]
    fn pool_matches_single_pass_conversion() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        };
        // Tamaños grandes (varios hilos), impares y con relleno de stride.
        for (w, h, pad) in [
            (1366usize, 768usize, 0usize),
            (1365, 767, 8),
            (801, 601, 16),
        ] {
            let stride = w * 4 + pad;
            let src: Vec<u8> = (0..stride * h).map(|_| next()).collect();
            let want = packed_to_i420(&src, stride, w as u32, h as u32, PackedRgb::Bgrx).unwrap();
            let mut pool = ConvertPool::for_pixels(w * h);
            // Dos veces: la segunda reutiliza los búferes de los hilos.
            let mut buf = src.clone();
            for round in 0..2 {
                let (back, got) = pool.convert(buf, stride, w as u32, h as u32, PackedRgb::Bgrx);
                assert_eq!(got.as_ref(), Some(&want), "{w}x{h} vuelta {round}");
                assert_eq!(back, src);
                buf = back;
            }
        }
    }

    #[test]
    fn tiny_and_invalid_images() {
        let mut pool = ConvertPool::for_pixels(4);
        assert_eq!(pool.threads(), 1);
        let px = vec![0u8; 2 * 2 * 4];
        let (_, got) = pool.convert(px.clone(), 8, 2, 2, PackedRgb::Bgrx);
        assert_eq!(got.unwrap().y, vec![16; 4]);
        let (_, got) = pool.convert(px, 8, 2, 3, PackedRgb::Bgrx);
        assert!(got.is_none());
    }
}
