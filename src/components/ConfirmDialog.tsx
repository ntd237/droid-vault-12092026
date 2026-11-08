import { useEffect, useMemo, useState } from "react";
import { getConfig } from "../lib/tauri";
import type { AppInfo, BatchProgress, BatchResult } from "../types";

interface ConfirmDialogProps {
  mode: "uninstall" | "restore";
  apps: AppInfo[];
  /** Number of apps actually targeted (restorable subset when restoring). */
  total: number;
  running: boolean;
  progress: BatchProgress | null;
  results: BatchResult[];
  summary: { success: number; failure: number } | null;
  /** Errors shown inside the dialog instead of a banner under the header. */
  error: string | null;
  labels: Map<string, string>;
  onConfirm: () => void;
  onCancel: () => void;
}

// The confirm dialog doubles as the batch progress/result surface: after the
// user confirms, the list collapses into a progress view and stays open until
// they dismiss it — no banner under the header pushes the app list around.
export default function ConfirmDialog({
  mode,
  apps,
  total,
  running,
  progress,
  results,
  summary,
  error,
  labels,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const [blacklist, setBlacklist] = useState<string[] | null>(null);

  // Pre-warning: check the blacklist once when the dialog opens.
  useEffect(() => {
    let cancelled = false;
    getConfig()
      .then((config) => {
        if (!cancelled) setBlacklist(config.blacklist);
      })
      .catch(() => {
        if (!cancelled) setBlacklist([]);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const blacklistedPkgs = useMemo(() => {
    if (!blacklist || blacklist.length === 0) return [];
    const set = new Set(blacklist);
    return apps.filter((a) => set.has(a.package)).map((a) => a.package);
  }, [blacklist, apps]);

  const hasSystemApp = apps.some((a) => a.kind === "System");
  const isUninstall = mode === "uninstall";
  const confirmPhase = !running && results.length === 0 && summary === null;
  const current = progress?.index ?? 0;
  const percent =
    total > 0 ? Math.min(100, Math.round((current / total) * 100)) : 0;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4">
      <div className="w-full max-w-lg rounded-xl border border-zinc-200 bg-white p-6 shadow-xl dark:border-zinc-700 dark:bg-zinc-900">
        {confirmPhase && error ? (
          // Standalone popup for upfront refusals (e.g. restoring user apps):
          // message only, no list or action buttons to re-trigger the batch.
          <>
            <h2 className="text-lg font-semibold text-zinc-900 dark:text-zinc-100">
              {isUninstall ? "Không thể gỡ cài đặt" : "Không thể khôi phục"}
            </h2>
            <div className="mt-3 rounded-lg border border-red-500/40 bg-red-500/10 p-3 text-sm text-red-600 dark:text-red-400">
              {error}
            </div>
            <div className="mt-5 flex justify-end">
              <button
                type="button"
                onClick={onCancel}
                className="rounded-lg border border-zinc-300 px-4 py-1.5 text-sm text-zinc-600 transition-colors hover:border-zinc-500 hover:text-zinc-900 dark:border-zinc-700 dark:text-zinc-300 dark:hover:text-zinc-100"
              >
                Đóng
              </button>
            </div>
          </>
        ) : confirmPhase ? (
          <>
            <h2 className="text-lg font-semibold text-zinc-900 dark:text-zinc-100">
              {isUninstall ? "Xác nhận gỡ cài đặt" : "Xác nhận khôi phục"}
            </h2>

            <p className="mt-2 text-sm text-zinc-600 dark:text-zinc-400">
              {isUninstall ? "Bạn sắp gỡ cài đặt" : "Bạn sắp khôi phục"}{" "}
              <span className="font-semibold text-zinc-900 dark:text-zinc-100">
                {apps.length}
              </span>{" "}
              ứng dụng sau:
            </p>

            <ul className="mt-2 max-h-48 overflow-y-auto rounded-lg border border-zinc-200 bg-zinc-50 p-2 dark:border-zinc-800 dark:bg-zinc-800/40">
              {apps.map((app) => (
                <li key={app.package} className="px-1 py-0.5 text-sm">
                  <span className="text-zinc-800 dark:text-zinc-200">{app.label}</span>{" "}
                  <span className="font-mono text-xs text-zinc-500">{app.package}</span>
                </li>
              ))}
            </ul>

            {isUninstall && hasSystemApp && (
              <div className="mt-3 rounded-lg border border-amber-500/40 bg-amber-500/10 p-3 text-sm text-amber-700 dark:text-amber-300">
                Bạn đang gỡ ứng dụng hệ thống — rủi ro làm lỗi thiết bị. Hãy chắc
                chắn bạn hiểu rõ.
              </div>
            )}

            {blacklistedPkgs.length > 0 && (
              <div className="mt-3 rounded-lg border border-red-500/40 bg-red-500/10 p-3 text-sm text-red-600 dark:text-red-400">
                Các ứng dụng sau thuộc danh sách cấm gỡ và sẽ bị bỏ qua:{" "}
                <span className="font-mono text-xs">{blacklistedPkgs.join(", ")}</span>
              </div>
            )}

            <div className="mt-5 flex justify-end gap-3">
              <button
                type="button"
                onClick={onCancel}
                className="rounded-lg border border-zinc-300 px-4 py-1.5 text-sm text-zinc-600 transition-colors hover:border-zinc-500 hover:text-zinc-900 dark:border-zinc-700 dark:text-zinc-300 dark:hover:text-zinc-100"
              >
                Hủy
              </button>
              <button
                type="button"
                onClick={onConfirm}
                className={`rounded-lg px-4 py-1.5 text-sm font-medium text-white transition-colors ${
                  isUninstall
                    ? "bg-red-600 hover:bg-red-500"
                    : "bg-sky-600 hover:bg-sky-500"
                }`}
              >
                Xác nhận
              </button>
            </div>
          </>
        ) : (
          <>
            <h2 className="text-lg font-semibold text-zinc-900 dark:text-zinc-100">
              {running
                ? isUninstall
                  ? "Đang gỡ cài đặt…"
                  : "Đang khôi phục…"
                : isUninstall
                  ? "Kết quả gỡ cài đặt"
                  : "Kết quả khôi phục"}
            </h2>

            {error && (
              <div className="mt-3 rounded-lg border border-red-500/40 bg-red-500/10 p-3 text-sm text-red-600 dark:text-red-400">
                {error}
              </div>
            )}

            {!error && (
              <>
                <div className="mt-3 flex items-center justify-between gap-2 text-sm">
                  <span className="text-zinc-600 dark:text-zinc-400">
                    {running ? "Đang xử lý…" : "Hoàn tất"}
                  </span>
                  <span className="font-mono text-zinc-500 dark:text-zinc-400">
                    {current}/{total}
                  </span>
                </div>

                <div className="mt-2 h-2 overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
                  <div
                    className="h-full rounded-full bg-red-600 transition-all"
                    style={{ width: `${percent}%` }}
                  />
                </div>

                {running && progress?.package && (
                  <p className="mt-2 truncate text-xs text-zinc-500 dark:text-zinc-400">
                    Đang xử lý: {labels.get(progress.package) ?? progress.package}
                  </p>
                )}

                {results.length > 0 && (
                  <ul className="mt-3 max-h-48 space-y-0.5 overflow-y-auto rounded-lg border border-zinc-200 bg-zinc-50 p-2 font-mono text-xs dark:border-zinc-800 dark:bg-zinc-800/40">
                    {results.map((r) => (
                      <li
                        key={r.package}
                        className={
                          r.success
                            ? "text-green-600 dark:text-green-400"
                            : "text-red-600 dark:text-red-400"
                        }
                      >
                        {r.success
                          ? `✓ ${labels.get(r.package) ?? r.package}`
                          : `✗ ${r.package}: ${r.message}`}
                      </li>
                    ))}
                  </ul>
                )}

                {summary && (
                  <p className="mt-3 text-sm text-zinc-600 dark:text-zinc-300">
                    Hoàn tất:{" "}
                    <span className="text-green-600 dark:text-green-400">
                      {summary.success} thành công
                    </span>
                    ,{" "}
                    <span className="text-red-600 dark:text-red-400">
                      {summary.failure} thất bại
                    </span>
                  </p>
                )}
              </>
            )}

            {!running && (
              <div className="mt-5 flex justify-end">
                <button
                  type="button"
                  onClick={onCancel}
                  className="rounded-lg border border-zinc-300 px-4 py-1.5 text-sm text-zinc-600 transition-colors hover:border-zinc-500 hover:text-zinc-900 dark:border-zinc-700 dark:text-zinc-300 dark:hover:text-zinc-100"
                >
                  Đóng
                </button>
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
