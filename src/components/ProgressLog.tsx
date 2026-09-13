import { useEffect } from "react";
import type { BatchProgress, BatchResult } from "../types";

// Auto-hide delay after a batch finishes — user-set policy (2s).
const AUTO_HIDE_MS = 2000;

interface ProgressLogProps {
  running: boolean;
  total: number;
  progress: BatchProgress | null;
  results: BatchResult[];
  labels: Map<string, string>;
  summary: { success: number; failure: number } | null;
  error: string | null;
  onDismiss: () => void;
}

export default function ProgressLog({
  running,
  total,
  progress,
  results,
  labels,
  summary,
  error,
  onDismiss,
}: ProgressLogProps) {
  const current = progress?.index ?? 0;
  const percent = total > 0 ? Math.min(100, Math.round((current / total) * 100)) : 0;

  // Auto-hide 2s after the batch finishes, success or failure alike — the
  // summary stays readable just long enough, then the panel clears itself.
  useEffect(() => {
    if (running) return;
    const timer = window.setTimeout(onDismiss, AUTO_HIDE_MS);
    return () => window.clearTimeout(timer);
  }, [running, onDismiss]);

  return (
    <div className="rounded-xl border border-zinc-800 bg-zinc-800/40 p-4">
      <div className="flex items-center justify-between gap-2 text-sm">
        <span className="font-medium text-zinc-200">
          {running ? "Đang xử lý…" : "Kết quả thao tác"}
        </span>
        <span className="font-mono text-zinc-400">
          {current}/{total}
        </span>
      </div>

      <div className="mt-2 h-2 overflow-hidden rounded-full bg-zinc-700">
        <div
          className="h-full rounded-full bg-red-600 transition-all"
          style={{ width: `${percent}%` }}
        />
      </div>

      {error && (
        <p className="mt-3 rounded-md border border-red-500/40 bg-red-500/10 px-3 py-1.5 text-sm text-red-400">
          {error}
        </p>
      )}

      {results.length > 0 && (
        <ul className="mt-3 max-h-40 space-y-0.5 overflow-y-auto font-mono text-xs">
          {results.map((r) => (
            <li key={r.package} className={r.success ? "text-green-400" : "text-red-400"}>
              {r.success
                ? `✓ ${labels.get(r.package) ?? r.package}`
                : `✗ ${r.package}: ${r.message}`}
            </li>
          ))}
        </ul>
      )}

      {summary && (
        <p className="mt-3 text-sm text-zinc-300">
          Hoàn tất: <span className="text-green-400">{summary.success} thành công</span>,{" "}
          <span className="text-red-400">{summary.failure} thất bại</span>
        </p>
      )}
    </div>
  );
}
