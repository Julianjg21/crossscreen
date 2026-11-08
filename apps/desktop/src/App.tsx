import { useCallback, useEffect, useState } from "react";
import {
  agent,
  call,
  errorMessage,
  isUnavailable,
  onAgentDisconnected,
  type AgentInfo,
  type AgentStatus,
} from "./api";
import ThisDevice from "./views/ThisDevice";
import Peers from "./views/Peers";
import Pair from "./views/Pair";
import Network from "./views/Network";
import VirtualMonitor from "./views/VirtualMonitor";

type ViewId = "device" | "peers" | "pair" | "network" | "monitor";

const NAV: { id: ViewId; label: string }[] = [
  { id: "device", label: "Este equipo" },
  { id: "peers", label: "Equipos emparejados" },
  { id: "pair", label: "Emparejar" },
  { id: "network", label: "Red local" },
  { id: "monitor", label: "Monitor virtual" },
];

type Conn =
  | { state: "connecting" }
  | { state: "connected"; info: AgentInfo; status: AgentStatus }
  | { state: "unavailable"; message: string };

export default function App() {
  const [conn, setConn] = useState<Conn>({ state: "connecting" });
  const [view, setView] = useState<ViewId>("device");
  const [starting, setStarting] = useState(false);
  const [pairTarget, setPairTarget] = useState<string | null>(null);

  const connect = useCallback(async (start = false) => {
    setConn({ state: "connecting" });
    try {
      const info = start ? await agent.start() : await agent.connect();
      const status = await call({ kind: "get_status" }, "status");
      setConn({ state: "connected", info, status });
    } catch (e) {
      setConn({ state: "unavailable", message: errorMessage(e) });
    }
  }, []);

  useEffect(() => {
    void connect();
    const un = onAgentDisconnected(() =>
      setConn({ state: "unavailable", message: "Se perdió la conexión con el agente." }),
    );
    return () => {
      void un.then((f) => f());
    };
  }, [connect]);

  // Cualquier vista que detecte que el agente se cayó lo notifica aquí.
  const onError = useCallback((e: unknown) => {
    if (isUnavailable(e)) {
      setConn({ state: "unavailable", message: errorMessage(e) });
    }
  }, []);

  const startAgent = async () => {
    setStarting(true);
    await connect(true);
    setStarting(false);
  };

  const goPair = (addr: string) => {
    setPairTarget(addr);
    setView("pair");
  };

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <span className="brand-mark" aria-hidden="true" />
          <span>CrossScreen</span>
        </div>
        <nav aria-label="Secciones">
          <ul>
            {NAV.map((n) => (
              <li key={n.id}>
                <button
                  type="button"
                  className="nav-item"
                  aria-current={conn.state === "connected" && view === n.id ? "page" : undefined}
                  disabled={conn.state !== "connected"}
                  onClick={() => setView(n.id)}
                >
                  {n.label}
                </button>
              </li>
            ))}
            <li>
              <button type="button" className="nav-item" disabled aria-disabled="true">
                Transmitir pantalla <span className="badge muted">Próximamente</span>
              </button>
            </li>
          </ul>
        </nav>
        <div className="agent-status" role="status" aria-live="polite">
          <span className={`dot ${conn.state}`} aria-hidden="true" />
          {conn.state === "connected" && <>Agente v{conn.info.agent_version}</>}
          {conn.state === "connecting" && <>Conectando…</>}
          {conn.state === "unavailable" && <>Agente no iniciado</>}
        </div>
      </aside>

      <main className="content">
        {conn.state === "connecting" && <p className="muted">Conectando con el agente…</p>}

        {conn.state === "unavailable" && (
          <section className="card empty-state" aria-labelledby="agent-down">
            <h1 id="agent-down">Agente no iniciado</h1>
            <p>
              El panel de control necesita el agente de CrossScreen
              (<code>crossscreen-agent</code>), que se ejecuta en segundo plano y gestiona la
              identidad, el emparejamiento y la red.
            </p>
            <p className="muted small">Detalle: {conn.message}</p>
            <div className="actions">
              <button type="button" className="primary" onClick={startAgent} disabled={starting}>
                {starting ? "Iniciando…" : "Iniciar agente"}
              </button>
              <button type="button" onClick={() => void connect()} disabled={starting}>
                Reintentar
              </button>
            </div>
          </section>
        )}

        {conn.state === "connected" && (
          <>
            {view === "device" && <ThisDevice status={conn.status} onError={onError} />}
            {view === "peers" && <Peers onError={onError} />}
            {view === "pair" && (
              <Pair
                onError={onError}
                initialAddr={pairTarget}
                onConsumeInitialAddr={() => setPairTarget(null)}
              />
            )}
            {view === "network" && (
              <Network status={conn.status} onError={onError} onPair={goPair} />
            )}
            {view === "monitor" && <VirtualMonitor status={conn.status} onError={onError} />}
          </>
        )}
      </main>
    </div>
  );
}
