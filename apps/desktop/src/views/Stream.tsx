import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import {
  call,
  errorMessage,
  onAgentEvent,
  type AgentStatus,
  type DiscoveredDevice,
  type LatencySummary,
  type ModeInfo,
  type PeerInfo,
  type ReceiverSessionInfo,
  type StreamModeInfo,
  type StreamProfileInfo,
  type StreamReportInfo,
  type StreamSessionInfo,
} from "../api";

interface Props {
  status: AgentStatus;
  onError: (e: unknown) => void;
  initialPeer?: string | null;
  initialAddr?: string | null;
  onConsumeInitial?: () => void;
}

const DEFAULT_PORT = 47810;

const RES_PRESETS: [number, number][] = [
  [1280, 720],
  [1366, 768],
  [1600, 900],
  [1920, 1080],
  [1920, 1200],
  [2560, 1440],
  [2560, 1600],
  [3840, 2160],
];

const RATES = [30, 60, 75, 120, 144];

function suggestedAddr(d: DiscoveredDevice): string | null {
  const port = d.session_port > 0 ? d.session_port : DEFAULT_PORT;
  const v4 = d.addresses.find((a) => !a.includes(":"));
  if (v4) return `${v4}:${port}`;
  const v6 = d.addresses[0];
  return v6 ? `[${v6}]:${port}` : null;
}

function formatLatency(l: LatencySummary | null): string {
  if (!l) return "—";
  const p50 = (l.p50_us / 1000).toFixed(2);
  const p95 = (l.p95_us / 1000).toFixed(2);
  const max = (l.max_us / 1000).toFixed(2);
  return `p50 ${p50} / p95 ${p95} / máx ${max} ms (n=${l.samples})`;
}

function formatElapsed(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return m > 0 ? `${m} min ${s} s` : `${s} s`;
}

export default function Stream({
  status,
  onError,
  initialPeer,
  initialAddr,
  onConsumeInitial,
}: Props) {
  const [peers, setPeers] = useState<PeerInfo[]>([]);
  const [devices, setDevices] = useState<Map<string, DiscoveredDevice>>(new Map());
  const [active, setActive] = useState<StreamSessionInfo | null>(status.active_stream);
  const [lastReport, setLastReport] = useState<StreamReportInfo | null>(status.last_stream);
  const [receiver, setReceiver] = useState<ReceiverSessionInfo | null>(status.active_receiver);
  const [selectedPeer, setSelectedPeer] = useState(initialPeer ?? "");
  const [addr, setAddr] = useState(initialAddr ?? "");
  const [mode, setMode] = useState<StreamModeInfo>("extend");
  const [profile, setProfile] = useState<StreamProfileInfo>("auto");
  const [resOption, setResOption] = useState("native");
  const [hz, setHz] = useState(60);
  const [mbps, setMbps] = useState("20");
  const [rxPort, setRxPort] = useState(String(DEFAULT_PORT));
  const [rxFullscreen, setRxFullscreen] = useState(true);
  const [busy, setBusy] = useState(false);
  const [rxBusy, setRxBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [nowSecs, setNowSecs] = useState(() => Math.floor(Date.now() / 1000));

  const loadState = useCallback(async () => {
    try {
      const [p, s] = await Promise.all([
        call({ kind: "list_peers" }, "peers"),
        call({ kind: "get_status" }, "status"),
      ]);
      setPeers(p.peers);
      setActive(s.active_stream);
      setLastReport(s.last_stream);
      setReceiver(s.active_receiver);
      if (!selectedPeer && p.peers.length > 0) {
        setSelectedPeer(p.peers[0].fingerprint);
      }
    } catch (e) {
      setError(errorMessage(e));
      onError(e);
    }
  }, [onError, selectedPeer]);

  useEffect(() => {
    if (initialPeer || initialAddr) {
      if (initialPeer) setSelectedPeer(initialPeer);
      if (initialAddr) setAddr(initialAddr);
      onConsumeInitial?.();
    }
  }, [initialPeer, initialAddr, onConsumeInitial]);

  useEffect(() => {
    void loadState();
    let alive = true;
    const un = onAgentEvent((ev) => {
      if (ev.kind === "stream_started") {
        setActive(ev.session);
        setError(null);
      } else if (ev.kind === "stream_ended") {
        setActive(null);
        setLastReport(ev.report);
        if (ev.report.error) {
          setError(ev.report.error);
        }
      } else if (ev.kind === "receiver_started") {
        setReceiver(ev.receiver);
        setError(null);
      } else if (ev.kind === "receiver_ended") {
        setReceiver(null);
        if (ev.message) {
          setError(ev.message);
        }
      } else if (ev.kind === "peers_changed") {
        void loadState();
      } else if (ev.kind === "device_discovered") {
        const { kind: _kind, ...dev } = ev;
        setDevices((m) => new Map(m).set(dev.fullname, dev));
      } else if (ev.kind === "device_lost") {
        setDevices((m) => {
          const n = new Map(m);
          n.delete(ev.fullname);
          return n;
        });
      }
    });
    if (status.mdns_available) {
      call({ kind: "start_discovery" }, "discovered")
        .then((r) => {
          if (!alive) return;
          setDevices((m) => {
            const n = new Map(m);
            for (const d of r.devices) n.set(d.fullname, d);
            return n;
          });
        })
        .catch(() => {
          // mDNS es opcional para transmitir si el usuario escribe IP:puerto.
        });
    }
    return () => {
      alive = false;
      void un.then((f) => f());
    };
  }, [loadState, status.mdns_available]);

  useEffect(() => {
    if (!active && !receiver) return;
    const t = setInterval(() => setNowSecs(Math.floor(Date.now() / 1000)), 1000);
    return () => clearInterval(t);
  }, [active, receiver]);

  // Si el par elegido se descubre en la red local y el campo dirección está vacío,
  // rellenarlo con la IP detectada y su puerto de sesión (o 47810 por defecto).
  const discoveredForSelected = useMemo(() => {
    if (!selectedPeer) return null;
    const deviceId = selectedPeer.slice(0, 32);
    for (const d of devices.values()) {
      if (d.device_id === deviceId) return d;
    }
    return null;
  }, [selectedPeer, devices]);

  useEffect(() => {
    if (!addr && discoveredForSelected) {
      const sug = suggestedAddr(discoveredForSelected);
      if (sug) setAddr(sug);
    }
  }, [addr, discoveredForSelected]);

  if (!status.streaming_supported) {
    return (
      <section aria-labelledby="h-stream">
        <h1 id="h-stream">Transmitir pantalla</h1>
        <div className="card empty-state">
          <p>Todavía no disponible en {status.platform}.</p>
          <p className="muted small">
            Por ahora la transmisión y recepción de pantalla solo están implementadas en Windows.
          </p>
        </div>
      </section>
    );
  }

  const start = async (ev: FormEvent) => {
    ev.preventDefault();
    setError(null);
    const mbpsNum = Number(mbps);
    if (!Number.isFinite(mbpsNum) || mbpsNum <= 0 || mbpsNum > 500) {
      setError("La tasa de bits debe estar entre 0,5 y 500 Mbps.");
      return;
    }
    let modeOverride: ModeInfo | null = null;
    if (resOption !== "native") {
      const [width, height] = resOption.split("x").map(Number);
      modeOverride = { width, height, refresh_mhz: hz * 1000 };
    }
    setBusy(true);
    try {
      const r = await call(
        {
          kind: "start_stream",
          addr: addr.trim(),
          peer: selectedPeer,
          mode,
          mode_override: modeOverride,
          bitrate_kbps: Math.round(mbpsNum * 1000),
          profile,
        },
        "stream_started",
      );
      setActive(r.session);
      setNowSecs(Math.floor(Date.now() / 1000));
    } catch (e) {
      setError(errorMessage(e));
      onError(e);
    } finally {
      setBusy(false);
    }
  };

  const stop = async () => {
    setBusy(true);
    setError(null);
    try {
      await call({ kind: "stop_stream" }, "ok");
    } catch (e) {
      setError(errorMessage(e));
      onError(e);
    } finally {
      setBusy(false);
    }
  };

  const startRx = async (ev: FormEvent) => {
    ev.preventDefault();
    setError(null);
    const portNum = Number(rxPort);
    if (!Number.isInteger(portNum) || portNum < 1 || portNum > 65535) {
      setError("El puerto UDP debe ser un número entre 1 y 65535.");
      return;
    }
    setRxBusy(true);
    try {
      const r = await call(
        { kind: "start_receiver", port: portNum, fullscreen: rxFullscreen },
        "receiver_started",
      );
      setReceiver(r.receiver);
      setNowSecs(Math.floor(Date.now() / 1000));
    } catch (e) {
      setError(errorMessage(e));
      onError(e);
    } finally {
      setRxBusy(false);
    }
  };

  const stopRx = async () => {
    setRxBusy(true);
    setError(null);
    try {
      await call({ kind: "stop_receiver" }, "ok");
    } catch (e) {
      setError(errorMessage(e));
      onError(e);
    } finally {
      setRxBusy(false);
    }
  };

  const elapsed = active ? Math.max(0, nowSecs - active.started_at) : 0;
  const rxElapsed = receiver ? Math.max(0, nowSecs - receiver.started_at) : 0;

  return (
    <section aria-labelledby="h-stream">
      <h1 id="h-stream">Transmitir o recibir pantalla</h1>
      <p className="lead">
        Extiende el escritorio de este PC hacia otro equipo emparejado, o convierte este equipo en
        pantalla receptora con un clic.
      </p>

      {error && (
        <p className="alert error" role="alert">
          {error}
        </p>
      )}

      {active ? (
        <div className="card" aria-live="polite">
          <h2>
            Transmisión en curso <span className="badge ok">En vivo</span>
          </h2>
          <dl className="props">
            <dt>Receptor</dt>
            <dd>
              {active.peer_name} (<code>{active.peer_fingerprint_short}</code>)
            </dd>
            <dt>Dirección</dt>
            <dd>
              <code>{active.remote_addr}</code>
            </dd>
            <dt>Modo</dt>
            <dd>
              {active.mode === "extend"
                ? "Extender escritorio (monitor virtual)"
                : "Duplicar pantalla principal"}
            </dd>
            <dt>Resolución pedida</dt>
            <dd>
              {active.mode_override
                ? `${active.mode_override.width} × ${active.mode_override.height} @ ${Math.round(active.mode_override.refresh_mhz / 1000)} Hz`
                : "Automática (modo nativo del receptor)"}
            </dd>
            <dt>RTT inicial</dt>
            <dd>{(active.initial_rtt_us / 1000).toFixed(2)} ms</dd>
            <dt>Tiempo activo</dt>
            <dd>{formatElapsed(elapsed)}</dd>
          </dl>
          <div className="actions" style={{ marginTop: 16 }}>
            <button type="button" className="danger" onClick={() => void stop()} disabled={busy}>
              {busy ? "Deteniendo…" : "Detener transmisión"}
            </button>
          </div>
        </div>
      ) : peers.length === 0 ? (
        <div className="card empty-state">
          <p>No hay equipos emparejados todavía.</p>
          <p className="muted small">
            Empareja primero los equipos en la sección «Emparejar» antes de transmitir o recibir.
          </p>
        </div>
      ) : (
        <form className="card" onSubmit={(e) => void start(e)}>
          <h2>Transmitir desde este equipo (Emisor)</h2>
          <div className="grid-2">
            <div>
              <label htmlFor="stream-peer">Equipo receptor (emparejado)</label>
              <select
                id="stream-peer"
                value={selectedPeer}
                onChange={(e) => {
                  setSelectedPeer(e.target.value);
                  const devId = e.target.value.slice(0, 32);
                  for (const d of devices.values()) {
                    if (d.device_id === devId) {
                      const sug = suggestedAddr(d);
                      if (sug) setAddr(sug);
                      break;
                    }
                  }
                }}
              >
                {peers.map((p) => (
                  <option key={p.fingerprint} value={p.fingerprint}>
                    {p.name} ({p.fingerprint_short})
                  </option>
                ))}
              </select>

              <label htmlFor="stream-addr">Dirección del visor (IP:puerto UDP)</label>
              <input
                id="stream-addr"
                value={addr}
                onChange={(e) => setAddr(e.target.value)}
                placeholder={`192.168.1.127:${DEFAULT_PORT}`}
                autoComplete="off"
                spellCheck={false}
                required
              />
              {discoveredForSelected && (
                <p className="muted small" style={{ marginTop: 4 }}>
                  Detectado en red local: {discoveredForSelected.addresses.join(", ")}
                  {discoveredForSelected.session_port > 0
                    ? ` (visor escuchando en puerto ${discoveredForSelected.session_port})`
                    : ""}
                </p>
              )}
            </div>

            <div>
              <label htmlFor="stream-mode">Modo de escritorio</label>
              <select
                id="stream-mode"
                value={mode}
                onChange={(e) => setMode(e.target.value as StreamModeInfo)}
              >
                <option value="extend">Extender escritorio (monitor virtual)</option>
                <option value="duplicate">Duplicar pantalla principal</option>
              </select>

              <div className="form-inline" style={{ marginTop: 12 }}>
                <div className="field" style={{ flex: 1 }}>
                  <label htmlFor="stream-res">Resolución</label>
                  <select
                    id="stream-res"
                    value={resOption}
                    onChange={(e) => setResOption(e.target.value)}
                  >
                    <option value="native">Modo nativo del receptor</option>
                    {RES_PRESETS.map(([w, h]) => (
                      <option key={`${w}x${h}`} value={`${w}x${h}`}>
                        {w} × {h}
                      </option>
                    ))}
                  </select>
                </div>
                {resOption !== "native" && (
                  <div className="field" style={{ minWidth: 110 }}>
                    <label htmlFor="stream-hz">Refresco</label>
                    <select
                      id="stream-hz"
                      value={hz}
                      onChange={(e) => setHz(Number(e.target.value))}
                    >
                      {RATES.map((r) => (
                        <option key={r} value={r}>
                          {r} Hz
                        </option>
                      ))}
                    </select>
                  </div>
                )}
              </div>

              <label htmlFor="stream-profile" style={{ marginTop: 12 }}>
                Perfil de red
              </label>
              <select
                id="stream-profile"
                value={profile}
                onChange={(e) => {
                  const p = e.target.value as StreamProfileInfo;
                  setProfile(p);
                  if (p === "auto") setMbps("20");
                  else if (p === "wifi") setMbps("12");
                  else if (p === "lan") setMbps("35");
                  else if (p === "saver") setMbps("5");
                }}
              >
                <option value="auto">Automático (escalado por resolución, recomendado)</option>
                <option value="wifi">Wi-Fi (resiliencia reforzada ante ráfagas)</option>
                <option value="lan">Cable Ethernet (baja latencia, alta calidad)</option>
                <option value="saver">Ahorro de datos (enlace débil)</option>
              </select>

              <label htmlFor="stream-mbps">Tasa objetivo H.264 (Mbps, a 1080p60)</label>
              <input
                id="stream-mbps"
                type="number"
                min="1"
                max="500"
                step="1"
                value={mbps}
                onChange={(e) => setMbps(e.target.value)}
                required
              />
            </div>
          </div>

          <div className="actions" style={{ marginTop: 18 }}>
            <button type="submit" className="primary" disabled={busy || !selectedPeer}>
              {busy ? "Conectando…" : "Iniciar transmisión"}
            </button>
          </div>
        </form>
      )}

      <div style={{ marginTop: 18 }}>
        {receiver ? (
          <div className="card" aria-live="polite">
            <h2>
              Recibiendo en este equipo <span className="badge info">Escuchando</span>
            </h2>
            <dl className="props">
              <dt>Escuchando en</dt>
              <dd>
                <code>UDP {receiver.listen_addr}</code> (anunciado por mDNS)
              </dd>
              <dt>Presentación</dt>
              <dd>{receiver.fullscreen ? "Pantalla completa" : "En ventana"}</dd>
              <dt>Proceso del visor</dt>
              <dd>
                PID <code>{receiver.pid}</code> · activo desde hace {formatElapsed(rxElapsed)}
              </dd>
            </dl>
            <div className="actions" style={{ marginTop: 16 }}>
              <button
                type="button"
                className="danger"
                onClick={() => void stopRx()}
                disabled={rxBusy}
              >
                {rxBusy ? "Cerrando…" : "Detener visor"}
              </button>
            </div>
          </div>
        ) : (
          <form className="card" onSubmit={(e) => void startRx(e)}>
            <h2>Recibir en este equipo (Usar este PC como monitor)</h2>
            <p className="muted small" style={{ marginTop: 0 }}>
              Abre el visor de CrossScreen esperando conexiones entrantes de tus equipos emparejados
              y anuncia el puerto automáticamente en la red local.
            </p>
            <div className="form-inline">
              <div className="field">
                <label htmlFor="rx-port">Puerto UDP</label>
                <input
                  id="rx-port"
                  type="number"
                  min="1"
                  max="65535"
                  value={rxPort}
                  onChange={(e) => setRxPort(e.target.value)}
                  required
                />
              </div>
              <div className="field">
                <label htmlFor="rx-mode">Modo de ventana</label>
                <select
                  id="rx-mode"
                  value={rxFullscreen ? "fullscreen" : "window"}
                  onChange={(e) => setRxFullscreen(e.target.value === "fullscreen")}
                >
                  <option value="fullscreen">Pantalla completa</option>
                  <option value="window">En ventana (1920 × 1080)</option>
                </select>
              </div>
              <div className="actions">
                <button
                  type="submit"
                  className="primary"
                  disabled={rxBusy || peers.length === 0}
                >
                  {rxBusy ? "Abriendo visor…" : "Activar modo pantalla"}
                </button>
              </div>
            </div>
          </form>
        )}
      </div>

      {lastReport && (
        <>
          <h2>Resumen de la última sesión emitida</h2>
          <div className="card">
            {lastReport.error && (
              <p className="alert error" role="alert">
                Fallo en la sesión: {lastReport.error}
              </p>
            )}
            <dl className="props">
              <dt>Receptor</dt>
              <dd>
                {lastReport.peer_name} (<code>{lastReport.remote_addr}</code>)
              </dd>
              <dt>Duración y fin</dt>
              <dd>
                {(lastReport.duration_ms / 1000).toFixed(1)} s — {lastReport.end_reason}
              </dd>
              {lastReport.captured_size && (
                <>
                  <dt>Tamaño capturado</dt>
                  <dd>
                    {lastReport.captured_size[0]} × {lastReport.captured_size[1]}
                  </dd>
                </>
              )}
              <dt>Frames (cap / cod / env)</dt>
              <dd>
                {lastReport.frames_captured} / {lastReport.frames_encoded} /{" "}
                {lastReport.frames_sent}
                {lastReport.duration_ms > 0 && (
                  <>
                    {" "}
                    (
                    {(
                      lastReport.frames_sent /
                      Math.max(0.001, lastReport.duration_ms / 1000)
                    ).toFixed(1)}{" "}
                    fps enviados)
                  </>
                )}
              </dd>
              <dt>Descartados</dt>
              <dd>
                antes de codificar: {lastReport.dropped_before_encode} · tras codificar:{" "}
                {lastReport.dropped_after_encode}
              </dd>
              <dt>IDR y recuperaciones</dt>
              <dd>
                {lastReport.idr_sent} IDR enviados · {lastReport.keyframe_requests} peticiones del
                receptor · {lastReport.reconnections ?? 0} reconexiones QUIC
              </dd>
              {lastReport.duration_ms > 0 && (
                <>
                  <dt>Bitrate real medido</dt>
                  <dd>
                    {(
                      (lastReport.payload_bytes * 8) /
                      (lastReport.duration_ms / 1000) /
                      1e6
                    ).toFixed(2)}{" "}
                    Mbps H.264 ·{" "}
                    {(
                      (lastReport.wire_bytes * 8) /
                      (lastReport.duration_ms / 1000) /
                      1e6
                    ).toFixed(2)}{" "}
                    Mbps en red (con FEC)
                  </dd>
                </>
              )}
              <dt>Datagramas QUIC</dt>
              <dd>
                {lastReport.datagrams_sent} enviados, {lastReport.datagrams_failed} fallidos
              </dd>
              <dt>Cursor y entrada</dt>
              <dd>
                {lastReport.cursor_positions} posiciones y {lastReport.cursor_shapes} formas
                enviadas · {lastReport.input_events_injected ?? 0} eventos de entrada remota
                inyectados
              </dd>
              <dt>Captura (CPU)</dt>
              <dd>{formatLatency(lastReport.capture_us)}</dd>
              <dt>Codificación (NV12+MFT)</dt>
              <dd>{formatLatency(lastReport.encode_us)}</dd>
              <dt>Captura → enviado (QPC)</dt>
              <dd>{formatLatency(lastReport.capture_to_sent_us)}</dd>
              {lastReport.receiver_frames_received !== null && (
                <>
                  <dt>Estadísticas del receptor</dt>
                  <dd>
                    recibidos {lastReport.receiver_frames_received} · perdidos{" "}
                    {lastReport.receiver_frames_lost ?? 0} · jitter{" "}
                    {((lastReport.receiver_jitter_us ?? 0) / 1000).toFixed(2)} ms · decodificación{" "}
                    {((lastReport.receiver_decode_us ?? 0) / 1000).toFixed(2)} ms
                  </dd>
                </>
              )}
            </dl>
          </div>
        </>
      )}
    </section>
  );
}
