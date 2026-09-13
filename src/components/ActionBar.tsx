interface ActionBarProps {
  selectedCount: number;
  running: boolean;
  /** true ⇔ the active tab's filtered list is non-empty (and not running) */
  canSelectAll: boolean;
  onSelectAll: () => void;
  onUninstall: () => void;
  onRestore: () => void;
  onClear: () => void;
}

export default function ActionBar({
  selectedCount,
  running,
  canSelectAll,
  onSelectAll,
  onUninstall,
  onRestore,
  onClear,
}: ActionBarProps) {
  const disabled = selectedCount === 0 || running;

  return (
    <div className="flex items-center gap-3">
      <span className="text-sm text-zinc-400">
        Đã chọn: <span className="font-semibold text-zinc-100">{selectedCount}</span>
      </span>

      <button
        type="button"
        onClick={onSelectAll}
        disabled={!canSelectAll}
        className="text-sm text-zinc-400 underline-offset-2 transition-colors hover:text-zinc-200 hover:underline disabled:cursor-not-allowed disabled:opacity-40"
      >
        Chọn tất cả
      </button>

      <button
        type="button"
        onClick={onUninstall}
        disabled={disabled}
        className="rounded-lg bg-red-600 px-4 py-1.5 text-sm font-medium text-white transition-colors hover:bg-red-500 disabled:cursor-not-allowed disabled:opacity-40"
      >
        Gỡ cài đặt
      </button>

      <button
        type="button"
        onClick={onRestore}
        disabled={disabled}
        className="rounded-lg border border-sky-600 px-4 py-1.5 text-sm font-medium text-sky-300 transition-colors hover:bg-sky-600/10 disabled:cursor-not-allowed disabled:opacity-40"
      >
        Khôi phục
      </button>

      {selectedCount > 0 && (
        <button
          type="button"
          onClick={onClear}
          disabled={running}
          className="text-sm text-zinc-400 underline-offset-2 transition-colors hover:text-zinc-200 hover:underline disabled:cursor-not-allowed disabled:opacity-40"
        >
          Bỏ chọn
        </button>
      )}

      {running && <span className="text-sm text-zinc-500">Đang thực hiện…</span>}
    </div>
  );
}
