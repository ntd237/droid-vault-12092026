import { useEffect, useMemo, useState } from "react";
import { getConfig } from "../lib/tauri";
import type { AppInfo } from "../types";

interface ConfirmDialogProps {
  mode: "uninstall" | "restore";
  apps: AppInfo[];
  onConfirm: () => void;
  onCancel: () => void;
}

export default function ConfirmDialog({
  mode,
  apps,
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

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4">
      <div className="w-full max-w-lg rounded-xl border border-zinc-700 bg-zinc-900 p-6 shadow-xl">
        <h2 className="text-lg font-semibold text-zinc-100">
          {isUninstall ? "Xác nhận gỡ cài đặt" : "Xác nhận khôi phục"}
        </h2>

        <p className="mt-2 text-sm text-zinc-400">
          {isUninstall ? "Bạn sắp gỡ cài đặt" : "Bạn sắp khôi phục"}{" "}
          <span className="font-semibold text-zinc-100">{apps.length}</span> ứng
          dụng sau:
        </p>

        <ul className="mt-2 max-h-48 overflow-y-auto rounded-lg border border-zinc-800 bg-zinc-800/40 p-2">
          {apps.map((app) => (
            <li key={app.package} className="px-1 py-0.5 text-sm">
              <span className="text-zinc-200">{app.label}</span>{" "}
              <span className="font-mono text-xs text-zinc-500">{app.package}</span>
            </li>
          ))}
        </ul>

        {isUninstall && hasSystemApp && (
          <div className="mt-3 rounded-lg border border-amber-500/40 bg-amber-500/10 p-3 text-sm text-amber-300">
            Bạn đang gỡ ứng dụng hệ thống — rủi ro làm lỗi thiết bị. Hãy chắc
            chắn bạn hiểu rõ.
          </div>
        )}

        {blacklistedPkgs.length > 0 && (
          <div className="mt-3 rounded-lg border border-red-500/40 bg-red-500/10 p-3 text-sm text-red-400">
            Các ứng dụng sau thuộc danh sách cấm gỡ và sẽ bị bỏ qua:{" "}
            <span className="font-mono text-xs">{blacklistedPkgs.join(", ")}</span>
          </div>
        )}

        <div className="mt-5 flex justify-end gap-3">
          <button
            type="button"
            onClick={onCancel}
            className="rounded-lg border border-zinc-700 px-4 py-1.5 text-sm text-zinc-300 transition-colors hover:border-zinc-500 hover:text-zinc-100"
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
      </div>
    </div>
  );
}
