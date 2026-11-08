import { useCallback, useEffect, useState } from "react";
import { call, errorMessage, onAgentEvent, type PeerInfo } from "../api";

interface Props {
  onError: (e: unknown) => void;
}

function formatDate(secs: number): string {
  if (!secs) return "—";
  return new Date(secs * 1000).toLocaleString("es");
}

export default function Peers({ onError }: Props) {
  const [peers, setPeers] = useState<PeerInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const r = await call({ kind: "list_peers" }, "peers");
      setPeers(r.peers);
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
      onError(e);
    }
  }, [onError]);

  useEffect(() => {
    void load();
    const un = onAgentEvent((ev) => {
      if (ev.kind === "peers_changed") void load();
    });
    return () => {
      void un.then((f) => f());
    };
  }, [load]);

  const forget = async (p: PeerInfo) => {
    setBusy(true);
    try {
      await call({ kind: "forget_peer", fingerprint: p.fingerprint }, "ok");
      setNotice(`Se olvidó «${p.name}».`);
      setConfirming(null);
      await load();
    } catch (e) {
      setError(errorMessage(e));
      onError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section aria-labelledby="h-peers">
      <h1 id="h-peers">Equipos emparejados</h1>
      <p className="lead">
        Equipos en los que este confía. Solo ellos podrán conectarse; olvidar uno obliga a
        emparejarlo de nuevo con PIN.
      </p>
      <div aria-live="polite">
        {notice && <p className="alert ok">{notice}</p>}
      </div>
      {error && (
        <p className="alert error" role="alert">
          {error}
        </p>
      )}
      {peers === null && !error && <p className="muted">Cargando…</p>}
      {peers && peers.length === 0 && (
        <div className="card empty-state">
          <p>No hay equipos emparejados todavía.</p>
          <p className="muted small">Ve a «Emparejar» para añadir uno.</p>
        </div>
      )}
      {peers && peers.length > 0 && (
        <table className="table">
          <thead>
            <tr>
              <th scope="col">Nombre</th>
              <th scope="col">Huella</th>
              <th scope="col">Emparejado</th>
              <th scope="col">
                <span className="visually-hidden">Acciones</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {peers.map((p) => (
              <tr key={p.fingerprint}>
                <td>{p.name}</td>
                <td>
                  <code title={p.fingerprint}>{p.fingerprint_short}</code>
                </td>
                <td>{formatDate(p.paired_at)}</td>
                <td className="row-actions">
                  {confirming === p.fingerprint ? (
                    <>
                      <button
                        type="button"
                        className="danger"
                        disabled={busy}
                        onClick={() => void forget(p)}
                      >
                        Confirmar
                      </button>
                      <button type="button" disabled={busy} onClick={() => setConfirming(null)}>
                        Cancelar
                      </button>
                    </>
                  ) : (
                    <button
                      type="button"
                      aria-label={`Olvidar ${p.name}`}
                      onClick={() => {
                        setNotice(null);
                        setConfirming(p.fingerprint);
                      }}
                    >
                      Olvidar
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}
