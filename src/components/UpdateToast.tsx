import type { UpdaterStatus } from "../hooks/useUpdater";

interface UpdateToastProps {
  status: UpdaterStatus;
  progress: number | null;
}

// Unobtrusive bottom-right toast shown only while an update is being
// downloaded, installed, or is about to relaunch the app. Idle renders nothing.
export default function UpdateToast({ status, progress }: UpdateToastProps) {
  if (
    status !== "downloading" &&
    status !== "installing" &&
    status !== "restarting" &&
    status !== "done"
  ) {
    return null;
  }

  const label =
    status === "done"
      ? "Đã cài bản mới — đang khởi động lại…"
      : status === "installing"
        ? "Đang cập nhật Droid Vault…"
        : `Đang cập nhật Droid Vault…${progress !== null ? ` ${progress}%` : ""}`;
  const width =
    status === "downloading" && progress !== null ? progress : 100;

  return (
    <div className="fixed bottom-4 right-4 z-50 rounded-xl border border-zinc-200 bg-white px-4 py-3 shadow-lg dark:border-zinc-800 dark:bg-zinc-900">
      <p className="text-sm text-zinc-700 dark:text-zinc-300">{label}</p>
      <div className="mt-2 h-1 w-48 overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
        <div
          className="h-full bg-red-600 transition-all duration-200"
          style={{ width: `${width}%` }}
        />
      </div>
    </div>
  );
}
