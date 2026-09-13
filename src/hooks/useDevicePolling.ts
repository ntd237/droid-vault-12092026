import { useEffect, useRef, useState } from "react";
import { listDevices } from "../lib/tauri";
import type { DeviceDto } from "../types";

export type DeviceStatus = "connected" | "unauthorized" | "none";

export interface DevicePollingState {
  status: DeviceStatus;
  device: DeviceDto | null;
  adbError: string | null;
  /** false until the first poll round has produced a result */
  ready: boolean;
}

const POLL_INTERVAL_MS = 2000;

function derive(devices: DeviceDto[]): { status: DeviceStatus; device: DeviceDto | null } {
  const connected = devices.find((d) => d.state === "device");
  if (connected) return { status: "connected", device: connected };
  const unauthorized = devices.find((d) => d.state === "unauthorized");
  if (unauthorized) return { status: "unauthorized", device: unauthorized };
  return { status: "none", device: null };
}

/**
 * Polls `list_devices` every 2s and derives the connection status.
 * Poll errors (e.g. adb missing) surface as `adbError` without breaking
 * the polling loop.
 */
export function useDevicePolling(): DevicePollingState {
  const [state, setState] = useState<DevicePollingState>({
    status: "none",
    device: null,
    adbError: null,
    ready: false,
  });
  const inFlight = useRef(false);

  useEffect(() => {
    let cancelled = false;

    async function poll() {
      // Skip a tick when the previous call is still running (adb can be slow).
      if (inFlight.current) return;
      inFlight.current = true;
      try {
        const devices = await listDevices();
        if (cancelled) return;
        const { status, device } = derive(devices);
        setState({ status, device, adbError: null, ready: true });
      } catch (err) {
        if (cancelled) return;
        setState(() => ({
          status: "none",
          device: null,
          adbError: err instanceof Error ? err.message : String(err),
          ready: true,
        }));
      } finally {
        inFlight.current = false;
      }
    }

    poll();
    const id = setInterval(poll, POLL_INTERVAL_MS);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);

  return state;
}
