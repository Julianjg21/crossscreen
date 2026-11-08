import { useEffect, useRef, useState, type FormEvent } from "react";
import { call, errorMessage, onAgentEvent, type IdentityInfo, type PeerInfo } from "../api";

interface Props {
  onError: (e: unknown) => void;
  initialAddr: string | null;
  onConsumeInitialAddr: () => void;
}

const PIN_VALIDITY_S = 120;

type Listen =
  | { state: "idle" }
  | { state: "starting" }
  | { state: "waiting"; pin: string; port: number; expiresAt: number | null }
  | { state: "paired"; peer: PeerInfo }
  | { state: "ended"; message: string; error: boolean };

type Join =
  | { state: "idle" }
  | { state: "pairing" }
  | { state: "paired"; peer: PeerInfo }
  | { state: "error"; message: string };

export default function Pair({ onError, initialAddr, onConsumeInitialAddr }: Props) {
  const [me, setMe] = useState<IdentityInfo | null>(null);
  const [listen, setListen] = useState<Listen>({ state: "idle" });
  const [join, setJoin] = useState<Join>({ state: "idle" });
  const [addr, setAddr] = useState(initialAddr ?? "");
  const [pin, setPin] = useState("");
  const [now, setNow] = useState(() => Date.now());
  const pinInput = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (initialAddr) {
      setAddr(initialAddr);
      onConsumeInitialAddr();
      pinInput.current?.focus();
    }
  }, [initialAddr, onConsumeInitialAddr]);

  useEffect(() => {
    let alive = true;
    call({ kind: "get_identity" }, "identity")
      .then((i) => alive && setMe(i))
      .catch(onError);
    // Si ya hay un PIN activo en el agente (p. ej. se reabrió la UI), recuperarlo.
    call({ kind: "get_status" }, "status")
      .then(async (s) => {
        if (s.pairing_port !== null && alive) {
          const r = await call({ kind: "start_pairing_listener" }, "pairing_started");
          if (alive) setListen({ state: "waiting", pin: r.pin, port: r.port, expiresAt: null });
        }
      })
      .catch(onError);
    const un = onAgentEvent((ev) => {
      if (ev.kind !== "pairing_result") return;
      switch (ev.outcome) {
        case "paired":
          setListen({ state: "paired", peer: ev.peer });
          break;
        case "failed":
          setListen({ state: "ended", message: ev.message, error: true });
          break;
        case "expired":
          setListen({ state: "ended", message: "El PIN caducó sin que nadie se emparejara.", error: true });
          break;
        case "cancelled":
          setListen({ state: "ended", message: "Emparejamiento cancelado.", error: false });
          break;
      }
    });
    return () => {
      alive = false;
      void un.then((f) => f());
    };
  }, [onError]);

  // Cuenta atrás del PIN.
  useEffect(() => {
    if (listen.state !== "waiting" || listen.expiresAt === null) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [listen]);

  const startListen = async () => {
    setListen({ state: "starting" });
    try {
      const r = await call({ kind: "start_pairing_listener" }, "pairing_started");
      setNow(Date.now());
      setListen({
        state: "waiting",
        pin: r.pin,
        port: r.port,
        expiresAt: Date.now() + PIN_VALIDITY_S * 1000,
      });
    } catch (e) {
      setListen({ state: "ended", message: errorMessage(e), error: true });
      onError(e);
    }
  };

  const cancelListen = async () => {
    try {
      await call({ kind: "cancel_pairing" }, "ok");
    } catch (e) {
      onError(e);
    }
  };

  const submitJoin = async (ev: FormEvent) => {
    ev.preventDefault();
    setJoin({ state: "pairing" });
    try {
      const r = await call({ kind: "pair_with", addr: addr.trim(), pin: pin.trim() }, "paired");
      setJoin({ state: "paired", peer: r.peer });
      setPin("");
    } catch (e) {
      setJoin({ state: "error", message: errorMessage(e) });
      onError(e);
    }
  };

  const remaining =
    listen.state === "waiting" && listen.expiresAt !== null
      ? Math.max(0, Math.ceil((listen.expiresAt - now) / 1000))
      : null;

  return (
    <section aria-labelledby="h-pair">
      <h1 id="h-pair">Emparejar</h1>
      <p className="lead">
        Empareja dos equipos una sola vez con un PIN de 6 dígitos. Uno muestra el PIN y el otro lo
        introduce junto con su dirección.
      </p>

      <div className="grid-2">
        <div className="card" aria-labelledby="h-show-pin">
          <h2 id="h-show-pin">Mostrar un PIN en este equipo</h2>
          <div aria-live="polite">
            {(listen.state === "idle" || listen.state === "starting") && (
              <>
                <p className="muted">
                  Abre un puerto de emparejamiento durante 2 minutos y muestra un PIN para
                  introducirlo en el otro equipo.
                </p>
                <button
                  type="button"
                  className="primary"
                  onClick={() => void startListen()}
                  disabled={listen.state === "starting"}
                >
                  {listen.state === "starting" ? "Abriendo…" : "Mostrar PIN"}
                </button>
              </>
            )}
            {listen.state === "waiting" && (
              <>
                <p className="pin" aria-label={`PIN ${listen.pin.split("").join(" ")}`}>
                  {listen.pin.slice(0, 3)} {listen.pin.slice(3)}
                </p>
                <dl className="props compact">
                  <dt>Puerto UDP</dt>
                  <dd>
                    <code>{listen.port}</code>
                  </dd>
                  {me && (
                    <>
                      <dt>Mi huella</dt>
                      <dd>
                        <code>{me.fingerprint_short}</code>
                      </dd>
                    </>
                  )}
                  {remaining !== null && (
                    <>
                      <dt>Caduca en</dt>
                      <dd>{remaining} s</dd>
                    </>
                  )}
                </dl>
                <p className="muted small">
                  En el otro equipo, introduce la IP de este equipo seguida de <code>:{listen.port}</code>{" "}
                  y el PIN. Este equipo también aparece en «Red local» del otro.
                </p>
                <button type="button" onClick={() => void cancelListen()}>
                  Cancelar
                </button>
              </>
            )}
            {listen.state === "paired" && (
              <>
                <p className="alert ok">
                  Emparejado con «{listen.peer.name}» (huella <code>{listen.peer.fingerprint_short}</code>).
                </p>
                <button type="button" onClick={() => setListen({ state: "idle" })}>
                  Listo
                </button>
              </>
            )}
            {listen.state === "ended" && (
              <>
                <p className={`alert ${listen.error ? "error" : "info"}`}>{listen.message}</p>
                <button type="button" onClick={() => setListen({ state: "idle" })}>
                  Volver
                </button>
              </>
            )}
          </div>
        </div>

        <form className="card" onSubmit={(e) => void submitJoin(e)} aria-labelledby="h-enter-pin">
          <h2 id="h-enter-pin">Introducir el PIN de otro equipo</h2>
          <label htmlFor="pair-addr">Dirección (IP:puerto)</label>
          <input
            id="pair-addr"
            value={addr}
            onChange={(e) => setAddr(e.target.value)}
            placeholder="192.168.1.20:50000"
            autoComplete="off"
            spellCheck={false}
            required
          />
          <label htmlFor="pair-pin">PIN</label>
          <input
            id="pair-pin"
            ref={pinInput}
            value={pin}
            onChange={(e) => setPin(e.target.value)}
            inputMode="numeric"
            pattern="[0-9 \-]{6,8}"
            maxLength={8}
            placeholder="123 456"
            autoComplete="off"
            required
          />
          <button type="submit" className="primary" disabled={join.state === "pairing"}>
            {join.state === "pairing" ? "Emparejando…" : "Emparejar"}
          </button>
          <div aria-live="polite">
            {join.state === "paired" && (
              <p className="alert ok">
                Emparejado con «{join.peer.name}». Comprueba que su huella{" "}
                <code>{join.peer.fingerprint_short}</code> coincide con la que muestra el otro
                equipo.
              </p>
            )}
            {join.state === "error" && (
              <p className="alert error" role="alert">
                {join.message}
              </p>
            )}
          </div>
        </form>
      </div>
    </section>
  );
}
