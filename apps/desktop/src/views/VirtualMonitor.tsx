import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import { call, errorMessage, type AgentStatus, type DisplayInfo } from "../api";

interface Props {
  status: AgentStatus;
  onError: (e: unknown) => void;
}

// Resoluciones habituales. El driver solo admite las que tenga configuradas;
// si una no está, el agente devuelve el error real del sistema.
const PRESETS: [number, number][] = [
  [1280, 720],
  [1600, 900],
  [1920, 1080],
  [1920, 1200],
  [2560, 1440],
  [2560, 1600],
  [3840, 2160],
];
const RATES = [30, 60, 75, 120, 144];

export default function VirtualMonitor({ status, onError }: Props) {
  const [displays, setDisplays] = useState<DisplayInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [res, setRes] = useState("1920x1080");
  const [hz, setHz] = useState(60);

  const load = useCallback(async () => {
    try {
      const r = await call({ kind: "list_displays" }, "displays");
      setDisplays(r.displays);
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
      onError(e);
    }
  }, [onError]);

  useEffect(() => {
    if (status.displays_supported) void load();
  }, [status.displays_supported, load]);

  // Si hay un monitor virtual activo, ofrecer los modos que anuncia de verdad.
  const advertised = useMemo(() => {
    const modes = (displays ?? []).filter((d) => d.is_virtual).flatMap((d) => d.modes);
    const seen = new Set<string>();
    return modes.filter((m) => {
      const k = `${m.width}x${m.height}@${Math.round(m.refresh_mhz / 1000)}`;
      if (seen.has(k)) return false;
      seen.add(k);
      return true;
    });
  }, [displays]);

  const resolutions = useMemo(() => {
    const set = new Set(PRESETS.map(([w, h]) => `${w}x${h}`));
    for (const m of advertised) set.add(`${m.width}x${m.height}`);
    return [...set].sort((a, b) => {
      const [aw, ah] = a.split("x").map(Number);
      const [bw, bh] = b.split("x").map(Number);
      return aw * ah - bw * bh;
    });
  }, [advertised]);

  if (!status.displays_supported) {
    return (
      <section aria-labelledby="h-mon">
        <h1 id="h-mon">Monitor virtual</h1>
        <div className="card empty-state">
          <p>Todavía no disponible en {status.platform}.</p>
          <p className="muted small">
            Por ahora el monitor virtual solo funciona en Windows (con Virtual-Display-Driver
            instalado mediante <code>crossscreen-helper</code>). El soporte para Linux está en el
            roadmap.
          </p>
        </div>
      </section>
    );
  }

  const virtuals = (displays ?? []).filter((d) => d.is_virtual);
  const anyActiveVirtual = virtuals.some((d) => d.active);

  const show = async (ev: FormEvent) => {
    ev.preventDefault();
    const [width, height] = res.split("x").map(Number);
    setBusy(true);
    setNotice(null);
    setError(null);
    try {
      const r = await call({ kind: "show_display", width, height, hz }, "display_shown");
      setNotice(
        `Monitor virtual activo en ${r.gdi_name} a ${r.mode.width}×${r.mode.height} @ ${Math.round(r.mode.refresh_mhz / 1000)} Hz.`,
      );
      await load();
    } catch (e) {
      setError(errorMessage(e));
      onError(e);
    } finally {
      setBusy(false);
    }
  };

  const hide = async () => {
    setBusy(true);
    setNotice(null);
    setError(null);
    try {
      const r = await call({ kind: "hide_displays" }, "displays_hidden");
      setNotice(
        r.count === 0 ? "No había monitores virtuales activos." : `Retirados ${r.count} monitor(es) virtual(es).`,
      );
      await load();
    } catch (e) {
      setError(errorMessage(e));
      onError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section aria-labelledby="h-mon">
      <h1 id="h-mon">Monitor virtual</h1>
      <p className="lead">
        Añade al escritorio de este equipo un monitor que Windows trata como real (aparece en
        Configuración de pantalla). Enviar su imagen a otro equipo aún no está disponible
        (próximamente).
      </p>
      <div aria-live="polite">{notice && <p className="alert ok">{notice}</p>}</div>
      {error && (
        <p className="alert error" role="alert">
          {error}
        </p>
      )}

      <form className="card form-inline" onSubmit={(e) => void show(e)}>
        <div className="field">
          <label htmlFor="mon-res">Resolución</label>
          <select id="mon-res" value={res} onChange={(e) => setRes(e.target.value)}>
            {resolutions.map((r) => (
              <option key={r} value={r}>
                {r.replace("x", " × ")}
              </option>
            ))}
          </select>
        </div>
        <div className="field">
          <label htmlFor="mon-hz">Frecuencia</label>
          <select id="mon-hz" value={hz} onChange={(e) => setHz(Number(e.target.value))}>
            {RATES.map((r) => (
              <option key={r} value={r}>
                {r} Hz
              </option>
            ))}
          </select>
        </div>
        <div className="actions">
          <button type="submit" className="primary" disabled={busy || virtuals.length === 0}>
            Mostrar monitor virtual
          </button>
          <button type="button" onClick={() => void hide()} disabled={busy || !anyActiveVirtual}>
            Retirar monitores virtuales
          </button>
          <button type="button" className="link" onClick={() => void load()} disabled={busy}>
            Actualizar
          </button>
        </div>
      </form>
      {displays && virtuals.length === 0 && (
        <p className="alert info">
          No se detecta ningún monitor virtual. Instala Virtual-Display-Driver con{" "}
          <code>crossscreen-helper</code> (requiere permisos de administrador una vez).
        </p>
      )}
      {advertised.length > 0 && (
        <p className="muted small">
          Modos que anuncia el monitor virtual activo:{" "}
          {advertised
            .map((m) => `${m.width}×${m.height}@${Math.round(m.refresh_mhz / 1000)}`)
            .join(", ")}
        </p>
      )}

      <h2>Pantallas conectadas</h2>
      {displays === null && !error && <p className="muted">Cargando…</p>}
      {displays && (
        <table className="table">
          <thead>
            <tr>
              <th scope="col">Nombre</th>
              <th scope="col">Tipo</th>
              <th scope="col">Estado</th>
              <th scope="col">Salida</th>
            </tr>
          </thead>
          <tbody>
            {displays.map((d) => (
              <tr key={d.device_path || d.friendly_name}>
                <td>{d.friendly_name || "(panel interno)"}</td>
                <td>{d.is_virtual ? <span className="badge info">Virtual</span> : "Física"}</td>
                <td>{d.active ? "Activa" : "Inactiva"}</td>
                <td>
                  <code>{d.gdi_name ?? "—"}</code>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}
