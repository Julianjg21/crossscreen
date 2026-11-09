import { useEffect, useState } from "react";
import {
  call,
  errorMessage,
  type AgentStatus,
  type IdentityInfo,
  type SettingsInfo,
} from "../api";

/** Puerto por defecto del visor receptor (el mismo que en «Recibir pantalla»). */
const DEFAULT_RX_PORT = 47810;

interface Props {
  status: AgentStatus;
  onError: (e: unknown) => void;
}

export default function ThisDevice({ status: initialStatus, onError }: Props) {
  const [id, setId] = useState<IdentityInfo | null>(null);
  const [status, setStatus] = useState<AgentStatus>(initialStatus);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [settings, setSettings] = useState<SettingsInfo | null>(null);
  const [settingsBusy, setSettingsBusy] = useState(false);
  const [settingsError, setSettingsError] = useState<string | null>(null);

  useEffect(() => {
    // Agentes anteriores a IPC 1.4 no conocen la petición: se omite la sección.
    call({ kind: "get_settings" }, "settings")
      .then((s) => setSettings(s))
      .catch(() => setSettings(null));
  }, []);

  const saveSettings = async (next: Omit<SettingsInfo, "autostart_supported">) => {
    setSettingsBusy(true);
    setSettingsError(null);
    try {
      const s = await call(
        {
          kind: "set_settings",
          autostart: next.autostart,
          receiver_on_start: next.receiver_on_start,
        },
        "settings",
      );
      setSettings(s);
    } catch (e) {
      setSettingsError(errorMessage(e));
    } finally {
      setSettingsBusy(false);
    }
  };

  useEffect(() => {
    let alive = true;
    Promise.all([call({ kind: "get_identity" }, "identity"), call({ kind: "get_status" }, "status")])
      .then(([i, s]) => {
        if (!alive) return;
        setId(i);
        setStatus(s);
      })
      .catch((e) => {
        if (!alive) return;
        setError(errorMessage(e));
        onError(e);
      });
    return () => {
      alive = false;
    };
  }, [onError]);

  const copy = async () => {
    if (!id) return;
    try {
      await navigator.clipboard.writeText(id.fingerprint);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      setCopied(false);
    }
  };

  return (
    <section aria-labelledby="h-device">
      <h1 id="h-device">Este equipo</h1>
      <p className="lead">
        Identidad criptográfica de este dispositivo. Los otros equipos la usan para reconocerlo tras
        emparejarse.
      </p>
      {error && (
        <p className="alert error" role="alert">
          {error}
        </p>
      )}
      {id && (
        <div className="card">
          <dl className="props">
            <dt>Nombre</dt>
            <dd>{id.name}</dd>
            <dt>Device ID</dt>
            <dd>
              <code>{id.device_id}</code>
            </dd>
            <dt>Huella</dt>
            <dd>
              <code className="fingerprint">{id.fingerprint_short}</code>
              <button type="button" className="link" onClick={copy}>
                {copied ? "Copiada" : "Copiar huella completa"}
              </button>
              <details>
                <summary>Ver huella completa (SHA-256)</summary>
                <code className="wrap">{id.fingerprint}</code>
              </details>
            </dd>
          </dl>
        </div>
      )}
      {settings && (
        <>
          <h2>Inicio</h2>
          <div className="card">
            <label style={{ display: "flex", gap: 8, alignItems: "flex-start" }}>
              <input
                type="checkbox"
                checked={settings.autostart}
                disabled={settingsBusy || !settings.autostart_supported}
                onChange={(e) =>
                  void saveSettings({
                    autostart: e.target.checked,
                    receiver_on_start: settings.receiver_on_start,
                  })
                }
              />
              <span>Iniciar CrossScreen al iniciar sesión (en segundo plano)</span>
            </label>
            {!settings.autostart_supported && (
              <p className="muted small" style={{ marginTop: 4 }}>
                No disponible con el AppImage: instala el paquete .deb.
              </p>
            )}
            <label style={{ display: "flex", gap: 8, alignItems: "flex-start", marginTop: 12 }}>
              <input
                type="checkbox"
                checked={settings.receiver_on_start !== null}
                disabled={settingsBusy}
                onChange={(e) =>
                  void saveSettings({
                    autostart: settings.autostart,
                    receiver_on_start: e.target.checked
                      ? { port: DEFAULT_RX_PORT, fullscreen: true, clipboard: false }
                      : null,
                  })
                }
              />
              <span>
                Quedar listo para recibir pantalla al iniciar (pantalla completa, puerto{" "}
                {DEFAULT_RX_PORT})
              </span>
            </label>
            <p className="muted small" style={{ marginTop: 4 }}>
              Con las dos opciones, este equipo funciona como pantalla nada más encenderlo: los
              equipos emparejados pueden transmitirle sin abrir el panel.
            </p>
            {settingsError && (
              <p className="alert error" role="alert">
                {settingsError}
              </p>
            )}
          </div>
        </>
      )}
      <h2>Agente</h2>
      <div className="card">
        <dl className="props">
          <dt>Versión</dt>
          <dd>
            {status.agent_version} (protocolo IPC {status.ipc_version.major}.
            {status.ipc_version.minor})
          </dd>
          <dt>Sistema</dt>
          <dd>{status.platform}</dd>
          <dt>mDNS</dt>
          <dd>
            {status.mdns_available ? "Anunciando este equipo en la red local" : "No disponible"}
          </dd>
          <dt>Datos</dt>
          <dd>
            <code className="wrap">{status.data_dir}</code>
          </dd>
          {status.log_file && (
            <>
              <dt>Registro</dt>
              <dd>
                <code className="wrap">{status.log_file}</code>
              </dd>
            </>
          )}
        </dl>
      </div>
    </section>
  );
}
