// TypeScript types mirroring the Tauri backend DTOs (src-tauri/src/commands.rs
// and src-tauri/src/packages.rs).

export type DeviceState = "device" | "unauthorized" | "offline";

export interface DeviceDto {
  serial: string;
  state: DeviceState;
  /** Vendor marketing name or model, null when unknown */
  model: string | null;
}

export type AppKind = "User" | "System";

export interface AppInfo {
  package: string;
  label: string;
  kind: AppKind;
  /** false ⇔ uninstalled for user 0 but restorable */
  installed: boolean;
  /** true ⇔ `cmd package install-existing` can restore it (system kind only) */
  restorable: boolean;
  icon_base64: string | null;
  version_code: number | null;
}

export interface BatchResult {
  package: string;
  success: boolean;
  message: string;
}

export interface BatchProgress {
  index: number;
  total: number;
  package: string;
}

export interface AppConfig {
  adb_path: string | null;
  blacklist: string[];
  theme: string;
}
