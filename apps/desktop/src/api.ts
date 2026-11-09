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
  streaming_supported: boolean;
  active_stream: StreamSessionInfo | null;
  last_stream: StreamReportInfo | null;
  active_receiver: ReceiverSessionInfo | null;
  data_dir: string;
  log_file: string | null;
  /** IPC 1.3: hace falta OpenH264 de Cisco (Linux) y dónde está instalado. */
  openh264_required?: boolean;
  openh264_path?: string | null;
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

export type StreamModeInfo = "extend" | "duplicate";
export type StreamProfileInfo = "auto" | "wifi" | "lan" | "saver";

export interface StreamSessionInfo {
  peer_name: string;
  peer_fingerprint: string;
  peer_fingerprint_short: string;
  remote_addr: string;
  mode: StreamModeInfo;
  mode_override: ModeInfo | null;
  bitrate_kbps: number;
  profile?: StreamProfileInfo;
  initial_rtt_us: number;
  started_at: number;
}

export interface LatencySummary {
  p50_us: number;
  p95_us: number;
  max_us: number;
  samples: number;
}

export interface StreamReportInfo {
  peer_name: string;
  remote_addr: string;
  duration_ms: number;
  end_reason: string;
  captured_size: [number, number] | null;
  frames_captured: number;
  frames_encoded: number;
  frames_sent: number;
  dropped_before_encode: number;
  dropped_after_encode: number;
  idr_sent: number;
  keyframe_requests: number;
  reconnections?: number;
  input_events_injected?: number;
  payload_bytes: number;
  wire_bytes: number;
  datagrams_sent: number;
  datagrams_failed: number;
  cursor_positions: number;
  cursor_shapes: number;
  capture_us: LatencySummary | null;
  encode_us: LatencySummary | null;
  capture_to_sent_us: LatencySummary | null;
  receiver_frames_received: number | null;
  receiver_frames_lost: number | null;
  receiver_jitter_us: number | null;
  receiver_decode_us: number | null;
  error: string | null;
}

/** IPC 1.4: recepción automática al arrancar el agente. */
export interface ReceiverAutoStartInfo {
  port: number;
  fullscreen: boolean;
  clipboard: boolean;
}

/** IPC 1.4: ajustes de arranque. */
export interface SettingsInfo {
  autostart: boolean;
  autostart_supported: boolean;
  receiver_on_start: ReceiverAutoStartInfo | null;
}

export interface ReceiverSessionInfo {
  listen_addr: string;
  port: number;
  fullscreen: boolean;
  pid: number;
  started_at: number;
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
  | { kind: "hide_displays" }
  | {
      kind: "start_stream";
      addr: string;
      peer: string;
      mode: StreamModeInfo;
      mode_override: ModeInfo | null;
      bitrate_kbps: number;
      profile: StreamProfileInfo;
      clipboard: boolean;
      accept_input?: boolean;
    }
  | { kind: "stop_stream" }
  | { kind: "start_receiver"; port: number; fullscreen: boolean; clipboard: boolean }
  | { kind: "stop_receiver" }
  | { kind: "install_open_h264" }
  | { kind: "get_settings" }
  | {
      kind: "set_settings";
      autostart: boolean;
      receiver_on_start: ReceiverAutoStartInfo | null;
    };

export type Response =
  | ({ kind: "status" } & AgentStatus)
  | ({ kind: "settings" } & SettingsInfo)
  | ({ kind: "identity" } & IdentityInfo)
  | { kind: "peers"; peers: PeerInfo[] }
  | { kind: "ok" }
  | { kind: "pairing_started"; pin: string; port: number }
  | { kind: "paired"; peer: PeerInfo }
  | { kind: "discovered"; devices: DiscoveredDevice[] }
  | { kind: "displays"; displays: DisplayInfo[] }
  | { kind: "display_shown"; gdi_name: string; mode: ModeInfo }
  | { kind: "displays_hidden"; count: number }
  | { kind: "stream_started"; session: StreamSessionInfo }
  | { kind: "receiver_started"; receiver: ReceiverSessionInfo };

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
  | { kind: "device_lost"; fullname: string }
  | { kind: "stream_started"; session: StreamSessionInfo }
  | { kind: "stream_ended"; report: StreamReportInfo }
  | { kind: "receiver_started"; receiver: ReceiverSessionInfo }
  | { kind: "receiver_ended"; message: string | null };

export interface CmdError {
  kind: "agent_unavailable" | "agent_error" | "protocol" | "local_error";
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

/** Abre la carpeta de registros en el explorador; devuelve su ruta. */
export const openLogsDir = () => invoke<string>("open_logs_dir");

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
