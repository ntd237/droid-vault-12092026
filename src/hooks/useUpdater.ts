import { useEffect, useState } from "react";

export type UpdaterStatus =
  | "idle"
  | "checking"
  | "downloading"
  | "installing"
  | "restarting"
  | "done";

// One auto-check per app launch; guards against double-running under React
// StrictMode's dev double-mount.
let ranOnce = false;

// Auto-update on startup: check for an update, download and install it
// without asking, then relaunch. Every failure is swallowed — the app must
// work normally when the update endpoint is unreachable (including dev,
// where the endpoint 404s). The plugin is desktop-only; dynamic imports keep
// it out of the main bundle and the try/catch absorbs any missing plugin.
export function useUpdater() {
  const [status, setStatus] = useState<UpdaterStatus>("idle");
  const [progress, setProgress] = useState<number | null>(null);

  useEffect(() => {
    if (ranOnce) return;
    ranOnce = true;
    let cancelled = false;

    const run = async () => {
      try {
        setStatus("checking");
        const { check } = await import("@tauri-apps/plugin-updater");
        const update = await check();
        if (!update) {
          setStatus("idle");
          return;
        }
        if (cancelled) return;

        setStatus("downloading");
        let total: number | null = null;
        let received = 0;
        await update.downloadAndInstall((event) => {
          if (event.event === "Started") {
            total =
              typeof event.data.contentLength === "number"
                ? event.data.contentLength
                : null;
            received = 0;
            setProgress(total !== null && total > 0 ? 0 : null);
          } else if (event.event === "Progress") {
            received += event.data.chunkLength;
            setProgress(
              total !== null && total > 0
                ? Math.min(100, Math.round((received / total) * 100))
                : null,
            );
          } else if (event.event === "Finished") {
            setStatus("installing");
          }
        });
        if (cancelled) return;

        setStatus("restarting");
        try {
          const { relaunch } = await import("@tauri-apps/plugin-process");
          await relaunch();
        } catch {
          // On Windows the installer exits the app itself, so relaunch may
          // never run here; nothing to recover either way.
        }
        setStatus("done");
      } catch (err) {
        // Silent by design: a failed update check must never bother the user.
        console.warn("Auto-update failed silently:", err);
        if (!cancelled) {
          setStatus("idle");
          setProgress(null);
        }
      }
    };

    void run();
    return () => {
      cancelled = true;
    };
  }, []);

  return { status, progress };
}
