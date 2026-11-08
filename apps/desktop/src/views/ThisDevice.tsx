import { useEffect, useState } from "react";
import { call, errorMessage, type AgentStatus, type IdentityInfo } from "../api";

interface Props {
  status: AgentStatus;
  onError: (e: unknown) => void;
}

export default function ThisDevice({ status: initialStatus, onError }: Props) {
  const [id, setId] = useState<IdentityInfo | null>(null);
  const [status, setStatus] = useState<AgentStatus>(initialStatus);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

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
