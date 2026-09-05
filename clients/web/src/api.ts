import { createAdministratorApiClient, type JsonGuard } from "@sarmg/admin-web";
import { ADMIN_AUTH_PATHS } from "@sarmg/contracts";

import protocolContract from "./protocol-contract.json";

const LOGIN_SUFFIX = "/auth/login";
if (!ADMIN_AUTH_PATHS.login.endsWith(LOGIN_SUFFIX)) {
  throw new Error("Foundation 管理登录路径不符合当前合同");
}
const foundationApiPrefix = ADMIN_AUTH_PATHS.login.slice(0, -LOGIN_SUFFIX.length);
if (protocolContract.api_prefix !== foundationApiPrefix) {
  throw new Error("Sentinel 产品协议必须与 Foundation 管理 API 使用同一前缀");
}

export const administratorApi = createAdministratorApiClient();

export type Camera = {
  id: string;
  name: string;
  location: string;
  has_sub_stream: boolean;
  onvif_configured: boolean;
  username: string | null;
  enabled: boolean;
  record_enabled: boolean;
  status: string;
  last_seen_at: string | null;
  created_at: string;
  updated_at: string;
};

export type CameraMutation = {
  camera: Camera;
  media_synced: boolean;
  warning: string | null;
  operation_id: string;
  operation_state: string;
};

export type MediaOperation = {
  id: string;
  camera_id: string;
  generation: number;
  kind: string;
  state: string;
  reason: string;
  requested_by: string | null;
  attempt: number;
  max_attempts: number;
  created_at: string;
  started_at: string | null;
  finished_at: string | null;
  retry_at: string | null;
  error_code: string | null;
  error_message: string | null;
};

export type StreamTicket = {
  profile: string;
  whep_url: string;
  hls_url: string;
  token: string;
  expires_at: string;
};

export type RecordingSpan = { start: string; duration: number };

export type MonitorEvent = {
  id: string;
  camera_id: string | null;
  kind: string;
  severity: "info" | "warning" | "critical";
  message: string;
  acknowledged_at: string | null;
  created_at: string;
};

export type AuditRow = {
  id: string;
  action: string;
  entity_type: string;
  entity_id: string | null;
  created_at: string;
};

export type SystemStatus = {
  service: string;
  version: string;
  database: string;
  media_service: string;
  cameras: { total: number; online: number; recording: number };
  server_time: string;
};

export type DiscoveredDevice = { xaddrs: string[] };

export function apiPath(path: string): string {
  if (!path.startsWith("/") || path.startsWith("//")) {
    throw new TypeError("业务 API 路径必须是单斜杠开头的绝对应用路径");
  }
  return `${protocolContract.api_prefix}${path}`;
}

export function request<T>(
  path: string,
  guard: JsonGuard<T>,
  init?: RequestInit,
): Promise<T> {
  return administratorApi.request(apiPath(path), guard, init);
}

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);
const isString = (value: unknown): value is string => typeof value === "string";
const isBoolean = (value: unknown): value is boolean => typeof value === "boolean";
const isNumber = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value);
const isNullableString = (value: unknown): value is string | null =>
  value === null || isString(value);
const arrayOf = <T>(guard: JsonGuard<T>): JsonGuard<T[]> =>
  (value): value is T[] => Array.isArray(value) && value.every(guard);

export const isUndefined = (value: unknown): value is undefined => value === undefined;

export const isCamera: JsonGuard<Camera> = (value): value is Camera =>
  isRecord(value) &&
  ["id", "name", "location", "status", "created_at", "updated_at"].every((key) =>
    isString(value[key]),
  ) &&
  isNullableString(value.username) &&
  isNullableString(value.last_seen_at) &&
  isBoolean(value.has_sub_stream) &&
  isBoolean(value.onvif_configured) &&
  isBoolean(value.enabled) &&
  isBoolean(value.record_enabled);

export const isCameras = arrayOf(isCamera);

export const isCameraMutation: JsonGuard<CameraMutation> = (
  value,
): value is CameraMutation =>
  isRecord(value) &&
  isCamera(value.camera) &&
  isBoolean(value.media_synced) &&
  isNullableString(value.warning) &&
  isString(value.operation_id) &&
  isString(value.operation_state);

export const isOperation: JsonGuard<MediaOperation> = (
  value,
): value is MediaOperation =>
  isRecord(value) &&
  ["id", "camera_id", "kind", "state", "reason", "created_at"].every((key) =>
    isString(value[key]),
  ) &&
  ["generation", "attempt", "max_attempts"].every((key) => isNumber(value[key])) &&
  [
    "requested_by",
    "started_at",
    "finished_at",
    "retry_at",
    "error_code",
    "error_message",
  ].every((key) => isNullableString(value[key]));

export const isStreamTicket: JsonGuard<StreamTicket> = (
  value,
): value is StreamTicket =>
  isRecord(value) &&
  ["profile", "whep_url", "hls_url", "token", "expires_at"].every((key) =>
    isString(value[key]),
  );

const isRecordingSpan: JsonGuard<RecordingSpan> = (
  value,
): value is RecordingSpan =>
  isRecord(value) && isString(value.start) && isNumber(value.duration);
export const isRecordingSpans = arrayOf(isRecordingSpan);

const isMonitorEvent: JsonGuard<MonitorEvent> = (
  value,
): value is MonitorEvent =>
  isRecord(value) &&
  ["id", "kind", "severity", "message", "created_at"].every((key) =>
    isString(value[key]),
  ) &&
  isNullableString(value.camera_id) &&
  isNullableString(value.acknowledged_at) &&
  ["info", "warning", "critical"].includes(value.severity as string);
export const isMonitorEvents = arrayOf(isMonitorEvent);

const isAuditRow: JsonGuard<AuditRow> = (value): value is AuditRow =>
  isRecord(value) &&
  ["id", "action", "entity_type", "created_at"].every((key) => isString(value[key])) &&
  isNullableString(value.entity_id);
export const isAuditRows = arrayOf(isAuditRow);

export const isSystemStatus: JsonGuard<SystemStatus> = (
  value,
): value is SystemStatus =>
  isRecord(value) &&
  ["service", "version", "database", "media_service", "server_time"].every((key) =>
    isString(value[key]),
  ) &&
  isRecord(value.cameras) &&
  isNumber(value.cameras.total) &&
  isNumber(value.cameras.online) &&
  isNumber(value.cameras.recording);

const isDiscoveredDevice: JsonGuard<DiscoveredDevice> = (
  value,
): value is DiscoveredDevice =>
  isRecord(value) && Array.isArray(value.xaddrs) && value.xaddrs.every(isString);
export const isDiscoveredDevices = arrayOf(isDiscoveredDevice);
