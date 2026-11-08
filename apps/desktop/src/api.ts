// Tipos y llamadas al agente. Reflejan exactamente `crates/cs-ipc/src/protocol.rs`
// (JSON con etiqueta `kind`). Si cambias uno, cambia el otro.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface IpcVersion {
  major: number;
  minor: number;
}

export interface AgentStatus {
  agent_version: string;
  ipc_version: IpcVersion;
  platform: string;
  pairing_port: number | null;
  discovery_active: boolean;
  mdns_available: boolean;
  displays_supported: boolean;
  data_dir: string;
  log_file: string | null;
}

export interface IdentityInfo {
  name: string;
  device_id: string;
  fingerprint: string;
  fingerprint_short: string;
}

export interface PeerInfo {
  name: string;
  fingerprint: string;
  fingerprint_short: string;
  paired_at: number;
}

export interface DiscoveredDevice {
  fullname: string;
  device_id: string;
  name: string;
  protocol_major: number;
  roles: string;
  addresses: string[];
  session_port: number;
  pairing_port: number | null;
  paired: boolean;
}

export interface ModeInfo {
  width: number;
  height: number;
  refresh_mhz: number;
}

export interface DisplayInfo {
  friendly_name: string;
  device_path: string;
  is_virtual: boolean;
  active: boolean;
  gdi_name: string | null;
  modes: ModeInfo[];
}

export type Request =
  | { kind: "get_status" }
  | { kind: "get_identity" }
  | { kind: "list_peers" }
  | { kind: "forget_peer"; fingerprint: string }
  | { kind: "start_pairing_listener" }
  | { kind: "cancel_pairing" }
  | { kind: "pair_with"; addr: string; pin: string }
  | { kind: "start_discovery" }
  | { kind: "list_displays" }
  | { kind: "show_display"; width: number; height: number; hz: number }
  | { kind: "hide_displays" };

export type Response =
  | ({ kind: "status" } & AgentStatus)
  | ({ kind: "identity" } & IdentityInfo)
  | { kind: "peers"; peers: PeerInfo[] }
  | { kind: "ok" }
  | { kind: "pairing_started"; pin: string; port: number }
  | { kind: "paired"; peer: PeerInfo }
  | { kind: "discovered"; devices: DiscoveredDevice[] }
  | { kind: "displays"; displays: DisplayInfo[] }
  | { kind: "display_shown"; gdi_name: string; mode: ModeInfo }
  | { kind: "displays_hidden"; count: number };

export type PairingOutcome =
  | { outcome: "paired"; peer: PeerInfo }
  | { outcome: "failed"; message: string }
  | { outcome: "cancelled" }
  | { outcome: "expired" };

export type AgentEvent =
  | { kind: "pairing_pin"; pin: string; port: number }
  | ({ kind: "pairing_result" } & PairingOutcome)
  | { kind: "peers_changed" }
  | ({ kind: "device_discovered" } & DiscoveredDevice)
  | { kind: "device_lost"; fullname: string };

export interface CmdError {
  kind: "agent_unavailable" | "agent_error" | "protocol";
  code: string | null;
  message: string;
}

export interface AgentInfo {
  agent_version: string;
  endpoint: string;
}

export function errorMessage(e: unknown): string {
  if (e && typeof e === "object" && "message" in e) {
    return String((e as { message: unknown }).message);
  }
  return String(e);
}

export function isUnavailable(e: unknown): boolean {
  return !!e && typeof e === "object" && (e as CmdError).kind === "agent_unavailable";
}

export const agent = {
  connect: () => invoke<AgentInfo>("agent_connect"),
  start: () => invoke<AgentInfo>("start_agent"),
  request: (request: Request) => invoke<Response>("agent_request", { request }),
};

/** Petición que espera un tipo de respuesta concreto. */
export async function call<K extends Response["kind"]>(
  request: Request,
  expected: K,
): Promise<Extract<Response, { kind: K }>> {
  const r = await agent.request(request);
  if (r.kind !== expected) {
    throw { kind: "protocol", code: null, message: `respuesta inesperada: ${r.kind}` } satisfies CmdError;
  }
  return r as Extract<Response, { kind: K }>;
}

export function onAgentEvent(cb: (e: AgentEvent) => void): Promise<UnlistenFn> {
  return listen<AgentEvent>("agent-event", (ev) => cb(ev.payload));
}

export function onAgentDisconnected(cb: () => void): Promise<UnlistenFn> {
  return listen("agent-disconnected", () => cb());
}

/** El device id son los primeros 16 bytes de la huella. */
export function isPairedDevice(deviceId: string, peers: PeerInfo[]): boolean {
  return peers.some((p) => p.fingerprint.startsWith(deviceId));
}
