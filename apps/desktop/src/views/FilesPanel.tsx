import { useEffect, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  call,
  errorMessage,
  onAgentEvent,
  openFilesDir,
  type FileTransferInfo,
} from "../api";

/** Máximo de transferencias que se muestran. */
const MAX_ITEMS = 20;

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

/**
 * Transferencia de archivos con el receptor durante una transmisión
 * (ADR-0010): se arrastran archivos a la ventana del panel y se ven los
 * enviados y recibidos.
 */
export default function FilesPanel() {
  const [items, setItems] = useState<FileTransferInfo[]>([]);
  const [over, setOver] = useState(false);
  const [queued, setQueued] = useState(0);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const un = onAgentEvent((ev) => {
      if (ev.kind === "file_transfer") {
        const { kind: _kind, ...info } = ev;
        setItems((l) => [info, ...l].slice(0, MAX_ITEMS));
        if (!info.incoming) setQueued((q) => Math.max(0, q - 1));
      }
    });
    return () => {
      void un.then((f) => f());
    };
  }, []);

  useEffect(() => {
    const un = getCurrentWebview().onDragDropEvent((e) => {
      const p = e.payload;
      if (p.type === "enter" || p.type === "over") {
        setOver(true);
      } else if (p.type === "leave") {
        setOver(false);
      } else if (p.type === "drop") {
        setOver(false);
        if (p.paths.length === 0) return;
        setError(null);
        call({ kind: "send_files", paths: p.paths }, "ok")
          .then(() => setQueued((q) => q + p.paths.length))
          .catch((err) => setError(errorMessage(err)));
      }
    });
    return () => {
      void un.then((f) => f());
    };
  }, []);

  return (
    <div style={{ marginTop: 16 }}>
      <h3 style={{ marginBottom: 8 }}>Archivos</h3>
      <div
        className="card"
        style={{
          borderStyle: "dashed",
          borderWidth: 2,
          textAlign: "center",
          padding: 16,
          opacity: over ? 1 : 0.85,
          outline: over ? "2px solid var(--accent, #3b82f6)" : "none",
        }}
        aria-live="polite"
      >
        {over
          ? "Suelta para enviar al receptor"
          : "Arrastra archivos a esta ventana para enviarlos al receptor"}
        {queued > 0 && <div className="muted small">{queued} en cola…</div>}
      </div>
      <p className="muted small" style={{ marginTop: 6 }}>
        En el receptor también se pueden soltar archivos en la ventana del visor para enviarlos
        aquí. Se guardan en Descargas/CrossScreen, nunca se sobrescriben y no se abren solos.{" "}
        <button
          type="button"
          className="link"
          onClick={() => openFilesDir().catch((e) => setError(errorMessage(e)))}
        >
          Abrir carpeta de recibidos
        </button>
      </p>
      {error && (
        <p className="alert error" role="alert">
          {error}
        </p>
      )}
      {items.length > 0 && (
        <ul className="small" style={{ paddingLeft: 18 }}>
          {items.map((it, i) => (
            <li key={i}>
              {it.incoming ? "Recibido" : "Enviado"}: <strong>{it.name}</strong>
              {it.error
                ? ` — falló: ${it.error}`
                : it.size !== null
                  ? ` (${formatBytes(it.size)})`
                  : ""}
              {it.path && (
                <>
                  {" "}
                  → <code>{it.path}</code>
                </>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
