import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AppConfig,
  AppInfo,
  BatchProgress,
  BatchResult,
  DeviceDto,
} from "../types";

// ---- Commands ----

export function listDevices(): Promise<DeviceDto[]> {
  return invoke("list_devices");
}

export function getApps(): Promise<AppInfo[]> {
  return invoke("get_apps");
}

export function uninstallApps(pkgs: string[]): Promise<BatchResult[]> {
  return invoke("uninstall_apps", { pkgs });
}

export function restoreApps(pkgs: string[]): Promise<BatchResult[]> {
  return invoke("restore_apps", { pkgs });
}

export function getConfig(): Promise<AppConfig> {
  return invoke("get_config");
}

// ---- Events ----

export function listenBatchProgress(
  handler: (payload: BatchProgress) => void,
): Promise<() => void> {
  return listen<BatchProgress>("batch-progress", (event) => handler(event.payload));
}

export function listenBatchResult(
  handler: (payload: BatchResult) => void,
): Promise<() => void> {
  return listen<BatchResult>("batch-result", (event) => handler(event.payload));
}

/// Background hydration stream: one event per app resolved after the fast
/// listing (icon/label filled in progressively).
export function listenAppResolved(
  handler: (payload: AppInfo) => void,
): Promise<() => void> {
  return listen<AppInfo>("app-resolved", (event) => handler(event.payload));
}
