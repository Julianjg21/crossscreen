import { useCallback, useEffect, useState } from "react";
import {
  bestAddr,
  call,
  errorMessage,
  isPairedDevice,
  onAgentEvent,
  type AgentStatus,
  type DiscoveredDevice,
  type PeerInfo,
} from "../api";

interface Props {
  status: AgentStatus;
  onError: (e: unknown) => void;
  onPair: (addr: string) => void;
  onStream: (peer?: string, addr?: string) => void;
}

function pairingAddr(d: DiscoveredDevice): string | null {
  if (d.pairing_port === null) return null;
  return bestAddr(d, d.pairing_port);
}

function streamAddr(d: DiscoveredDevice): string | null {
  return bestAddr(d, d.session_port > 0 ? d.session_port : 47810);
}

export default function Network({ status, onError, onPair, onStream }: Props) {
  const [devices, setDevices] = useState<Map<string, DiscoveredDevice>>(new Map());
  const [peers, setPeers] = useState<PeerInfo[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [started, setStarted] = useState(false);

  const loadPeers = useCallback(() => {
    call({ kind: "list_peers" }, "peers")
      .then((r) => setPeers(r.peers))
      .catch(onError);
  }, [onError]);

  useEffect(() => {
    if (!status.mdns_available) return;
    let alive = true;
    // Suscribirse antes de pedir la instantánea para no perder eventos.
    const un = onAgentEvent((ev) => {
      if (ev.kind === "device_discovered") {
        const { kind: _kind, ...dev } = ev;
        setDevices((m) => new Map(m).set(dev.fullname, dev));
      } else if (ev.kind === "device_lost") {
        setDevices((m) => {
          const n = new Map(m);
          n.delete(ev.fullname);
          return n;
        });
      } else if (ev.kind === "peers_changed") {
        loadPeers();
      }
    });
    call({ kind: "start_discovery" }, "discovered")
      .then((r) => {
        if (!alive) return;
        setDevices((m) => {
          const n = new Map(m);
          for (const d of r.devices) n.set(d.fullname, d);
          return n;
        });
        setStarted(true);
      })
      .catch((e) => {
        if (!alive) return;
        setError(errorMessage(e));
        onError(e);
      });
    loadPeers();
    return () => {
      alive = false;
      void un.then((f) => f());
    };
  }, [status.mdns_available, onError, loadPeers]);

  const list = [...devices.values()].sort((a, b) => a.name.localeCompare(b.name));

  return (
    <section aria-labelledby="h-net">
      <h1 id="h-net">Red local</h1>
      <p className="lead">
        Equipos con CrossScreen detectados en esta red mediante mDNS. La lista se actualiza sola.
      </p>
      {!status.mdns_available && (
        <p className="alert error">El servicio mDNS no pudo iniciarse en este equipo.</p>
      )}
      {error && (
        <p className="alert error" role="alert">
          {error}
        </p>
      )}
      {status.mdns_available && started && list.length === 0 && (
        <div className="card empty-state" aria-live="polite">
          <p>
            <span className="spinner" aria-hidden="true" /> Buscando equipos…
          </p>
          <p className="muted small">
            Si no aparece ninguno, comprueba que el otro equipo tenga el agente de CrossScreen en
            marcha, que estéis en la misma red y que el firewall o el punto de acceso permitan
            multicast (mDNS, UDP 5353).
          </p>
        </div>
      )}
      {list.length > 0 && (
        <ul className="device-list" aria-live="polite">
          {list.map((d) => {
            const paired = isPairedDevice(d.device_id, peers);
            const matchedPeer = peers.find((p) => p.fingerprint.startsWith(d.device_id));
            const addr = pairingAddr(d);
            const sAddr = streamAddr(d);
            return (
              <li key={d.fullname} className="card device">
                <div className="device-main">
                  <h2 className="device-name">
                    {d.name}
                    {paired && <span className="badge ok">Emparejado</span>}
                    {d.session_port > 0 && (
                      <span className="badge info">Visor en :{d.session_port}</span>
                    )}
                    {d.pairing_port !== null && (
                      <span className="badge info">Mostrando PIN</span>
                    )}
                  </h2>
                  <p className="muted small">
                    {d.addresses.join(", ") || "sin dirección"} · ID <code>{d.device_id.slice(0, 16)}…</code>
                  </p>
                </div>
                <div className="actions">
                  {paired && matchedPeer && (
                    <button
                      type="button"
                      className="primary"
                      onClick={() => onStream(matchedPeer.fingerprint, sAddr ?? undefined)}
                    >
                      Transmitir…
                    </button>
                  )}
                  {addr && !paired && (
                    <button type="button" className="primary" onClick={() => onPair(addr)}>
                      Emparejar…
                    </button>
                  )}
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
