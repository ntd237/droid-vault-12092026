import type { DevicePollingState } from "../hooks/useDevicePolling";

const STATUS_STYLES = {
  connected: "bg-green-500",
  unauthorized: "bg-amber-500",
  none: "bg-zinc-500",
} as const;

export default function StatusBar({ status, device, adbError }: DevicePollingState) {
  return (
    <div className="flex items-center gap-2 text-sm">
      <span className={`inline-block h-2.5 w-2.5 rounded-full ${STATUS_STYLES[status]}`} />
      {status === "connected" && (
        <span className="text-zinc-200">
          Đã kết nối: {device?.model ?? device?.serial}
        </span>
      )}
      {status === "unauthorized" && (
        <span className="text-amber-300">
          Chưa được phép — Mở điện thoại và bấm &laquo;Cho phép&raquo; để xác nhận gỡ lỗi USB
        </span>
      )}
      {status === "none" && (
        <span className="text-zinc-400">
          Chưa có thiết bị — Bật &laquo;Gỡ lỗi USB&raquo; trên điện thoại và cắm cáp
        </span>
      )}
      {adbError && (
        <span className="rounded-md border border-red-500/40 bg-red-500/10 px-2 py-0.5 text-red-400">
          {adbError}
        </span>
      )}
    </div>
  );
}
