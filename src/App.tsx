import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import appIcon from "./assets/app-icon.png";
import ActionBar from "./components/ActionBar";
import AppList from "./components/AppList";
import AppTabs from "./components/AppTabs";
import ConfirmDialog from "./components/ConfirmDialog";
import ProgressLog from "./components/ProgressLog";
import StatusBar from "./components/StatusBar";
import { useDevicePolling } from "./hooks/useDevicePolling";
import {
  getApps,
  listenAppResolved,
  listenBatchProgress,
  listenBatchResult,
  restoreApps,
  uninstallApps,
} from "./lib/tauri";
import type { AppInfo, AppKind, BatchProgress, BatchResult } from "./types";
import "./App.css";

function DevicePlaceholder({ status }: { status: "unauthorized" | "none" }) {
  if (status === "unauthorized") {
    return (
      <div className="max-w-xl rounded-xl border border-amber-500/30 bg-zinc-800/40 p-6">
        <h2 className="text-lg font-semibold text-amber-300">Chưa được phép</h2>
        <p className="mt-2 text-zinc-400">
          Mở điện thoại và bấm &laquo;Cho phép&raquo; để xác nhận gỡ lỗi USB.
        </p>
      </div>
    );
  }

  return (
    <div className="max-w-xl rounded-xl border border-zinc-800 bg-zinc-800/40 p-6">
      <h2 className="text-lg font-semibold text-zinc-200">Chưa có thiết bị</h2>
      <p className="mt-2 text-zinc-400">
        Bật &laquo;Gỡ lỗi USB&raquo; trên điện thoại và cắm cáp để bắt đầu.
      </p>
    </div>
  );
}

function Spinner({ className = "h-5 w-5" }: { className?: string }) {
  return (
    <span
      aria-hidden
      className={`inline-block animate-spin rounded-full border-2 border-zinc-500 border-t-transparent ${className}`}
    />
  );
}

function App() {
  const { status, device, adbError, ready } = useDevicePolling();

  const [apps, setApps] = useState<AppInfo[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [scanError, setScanError] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState<AppKind>("User");
  const [search, setSearch] = useState("");
  const [selected, setSelected] = useState<Set<string>>(new Set());

  const [confirmMode, setConfirmMode] = useState<"uninstall" | "restore" | null>(null);
  const [running, setRunning] = useState(false);
  const [batchTotal, setBatchTotal] = useState(0);
  const [batchProgress, setBatchProgress] = useState<BatchProgress | null>(null);
  const [batchResults, setBatchResults] = useState<BatchResult[]>([]);
  const [batchSummary, setBatchSummary] = useState<{
    success: number;
    failure: number;
  } | null>(null);
  const [batchError, setBatchError] = useState<string | null>(null);
  const [logDismissed, setLogDismissed] = useState(false);
  const dismissLog = useCallback(() => setLogDismissed(true), []);

  // Auto-rescan trigger: bump an epoch each time the device (re)connects so a
  // failed first scan is retried on the next connection.
  const prevStatus = useRef(status);
  const [connEpoch, setConnEpoch] = useState(0);
  useEffect(() => {
    if (prevStatus.current !== "connected" && status === "connected") {
      setConnEpoch((c) => c + 1);
    }
    prevStatus.current = status;
  }, [status]);

  const scanIdRef = useRef(0);
  const loadApps = useCallback(async () => {
    const id = ++scanIdRef.current;
    setLoading(true);
    setScanError(null);
    try {
      let result = await getApps();
      // A connected device never legitimately reports zero apps. An empty
      // roster means the scan raced a busy adb session — retry briefly
      // instead of dead-ending on an empty, unrecoverable list.
      for (let attempt = 1; result.length === 0 && attempt <= 2; attempt++) {
        if (scanIdRef.current !== id) return;
        await new Promise((resolve) => setTimeout(resolve, 1000 * attempt));
        if (scanIdRef.current !== id) return;
        result = await getApps();
      }
      if (scanIdRef.current !== id) return;
      if (result.length === 0) {
        // Still empty after retries: surface as an error so the scan bar
        // (and its Làm mới button) stays visible for a manual retry.
        setScanError("Quét được danh sách rỗng — hãy thử Làm mới");
      }
      setApps(result);
    } catch (err) {
      if (scanIdRef.current !== id) return;
      // Keep already-loaded apps; surface the failure as a chip.
      setScanError(err instanceof Error ? err.message : String(err));
    } finally {
      if (scanIdRef.current === id) setLoading(false);
    }
  }, []);

  // Background scan when the device connects and apps are not loaded yet.
  useEffect(() => {
    if (status === "connected" && apps === null && !loading) {
      void loadApps();
    }
  }, [status, apps, loading, connEpoch, loadApps]);

  // Progressive hydration: the backend streams each app it resolves after
  // the fast listing; merge them into the visible rows in place.
  useEffect(() => {
    const unlisten = listenAppResolved((info) => {
      setApps((prev) => {
        if (!prev) return prev;
        const idx = prev.findIndex((a) => a.package === info.package);
        if (idx === -1) return prev;
        const next = [...prev];
        next[idx] = info;
        return next;
      });
    });
    return () => {
      void unlisten.then((f) => f());
    };
  }, []);

  const toggleApp = useCallback((pkg: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(pkg)) {
        next.delete(pkg);
      } else {
        next.add(pkg);
      }
      return next;
    });
  }, []);

  const userCount = useMemo(
    () => (apps ?? []).filter((a) => a.kind === "User").length,
    [apps],
  );
  const systemCount = useMemo(
    () => (apps ?? []).filter((a) => a.kind === "System").length,
    [apps],
  );

  const filteredApps = useMemo(() => {
    const q = search.trim().toLowerCase();
    return (apps ?? [])
      .filter(
        (a) =>
          a.kind === activeTab &&
          (!q ||
            a.label.toLowerCase().includes(q) ||
            a.package.toLowerCase().includes(q)),
      )
      .sort(
        (a, b) =>
          a.label.localeCompare(b.label, undefined, { sensitivity: "base" }) ||
          a.package.localeCompare(b.package),
      );
  }, [apps, activeTab, search]);

  // Select-all adds every currently-filtered package of the ACTIVE tab.
  const selectAll = useCallback(() => {
    setSelected((prev) => {
      const next = new Set(prev);
      for (const app of filteredApps) next.add(app.package);
      return next;
    });
  }, [filteredApps]);

  const selectedApps = useMemo(
    () => (apps ?? []).filter((a) => selected.has(a.package)),
    [apps, selected],
  );
  // Labels for the result log; built from the full list so entries stay
  // valid after the selection is cleared at the end of a batch.
  const appLabels = useMemo(
    () => new Map((apps ?? []).map((a) => [a.package, a.label])),
    [apps],
  );

  const runBatch = useCallback(
    async (mode: "uninstall" | "restore") => {
      if (running || selected.size === 0) return;
      const pkgs = Array.from(selected);
      // install-existing only restores system packages (backend flags them
      // `restorable`); refuse upfront instead of failing on the device.
      const targets =
        mode === "restore"
          ? pkgs.filter((p) => apps?.some((a) => a.package === p && a.restorable))
          : pkgs;
      if (targets.length === 0) {
        setConfirmMode(null);
        setBatchError(
          "Không thể khôi phục: chỉ app hệ thống mới khôi phục được qua ADB. App người dùng (cài qua cửa hàng) khi gỡ sẽ không còn bản sao trên máy.",
        );
        return;
      }

      setConfirmMode(null);
      setRunning(true);
      setBatchTotal(targets.length);
      setBatchProgress(null);
      setBatchResults([]);
      setBatchSummary(null);
      setBatchError(null);
      setLogDismissed(false);

      const unlistenProgress = listenBatchProgress((p) => setBatchProgress(p));
      const unlistenResult = listenBatchResult((r) =>
        setBatchResults((prev) => [...prev, r]),
      );

      try {
        const results =
          mode === "uninstall"
            ? await uninstallApps(targets)
            : await restoreApps(targets);
        setBatchResults(results);
        const success = results.filter((r) => r.success).length;
        setBatchSummary({ success, failure: results.length - success });
        setSelected(new Set());
        // Flip the affected rows in place — a full rescan after every batch
        // re-pulls uncached APKs and stalls the UI for seconds.
        setApps((prev) =>
          prev
            ? prev.map((a) =>
                results.some((r) => r.package === a.package && r.success)
                  ? { ...a, installed: mode === "uninstall" ? false : true }
                  : a,
              )
            : prev,
        );
      } catch (err) {
        setBatchError(err instanceof Error ? err.message : String(err));
      } finally {
        (await unlistenProgress)();
        (await unlistenResult)();
        setRunning(false);
      }
    },
    [running, selected, apps],
  );

  const deviceLost = apps !== null && status !== "connected";
  const hasList = apps !== null || (loading && status === "connected");

  return (
    <div className="flex h-screen flex-col bg-zinc-900 text-zinc-100">
      <header className="flex items-center justify-between gap-4 border-b border-zinc-800 px-4 py-3">
        <div className="flex items-center gap-3">
          <img src={appIcon} alt="Droid Vault Icon" className="h-7 w-7 rounded-md object-contain" />
          <h1 className="text-lg font-semibold tracking-wide">Droid Vault</h1>
        </div>
        <StatusBar status={status} device={device} adbError={adbError} ready={ready} />
      </header>

      <main className="flex min-h-0 flex-1 flex-col p-4">
        {(running || batchResults.length > 0 || batchSummary || batchError) && !logDismissed && (
          <div className="mb-3">
            <ProgressLog
              running={running}
              total={batchTotal}
              progress={batchProgress}
              results={batchResults}
              labels={appLabels}
              summary={batchSummary}
              error={batchError}
              onDismiss={dismissLog}
            />
          </div>
        )}

        {!ready && !hasList && (
          <div className="flex flex-1 items-center justify-center">
            <p className="text-zinc-400">Đang quét thiết bị…</p>
          </div>
        )}

        {ready && !hasList && status !== "connected" && (
          <div className="flex flex-1 items-center justify-center">
            <DevicePlaceholder status={status === "unauthorized" ? "unauthorized" : "none"} />
          </div>
        )}

        {hasList && (
          <>
            {(apps !== null && (loading || scanError || deviceLost)) && (
              <div className="mb-3 flex flex-wrap items-center gap-2 text-sm">
                {loading && apps !== null && (
                  <span className="flex items-center gap-2 rounded-md border border-zinc-700 bg-zinc-800/60 px-3 py-1 text-zinc-300">
                    <Spinner className="h-3.5 w-3.5" />
                    Đang tải danh sách ứng dụng…
                  </span>
                )}
                {deviceLost && (
                  <span className="rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-1 text-amber-300">
                    Thiết bị đã ngắt kết nối — danh sách đã tải vẫn được giữ
                  </span>
                )}
                {scanError && (
                  <span className="rounded-md border border-red-500/40 bg-red-500/10 px-3 py-1 text-red-400">
                    Lỗi quét: {scanError}
                  </span>
                )}
              </div>
            )}

            {apps === null ? (
              <div className="flex flex-1 items-center justify-center">
                <div className="flex flex-col items-center gap-3">
                  <Spinner />
                  <p className="text-zinc-400">
                    Đang tải danh sách ứng dụng… Lần đầu có thể mất vài phút
                  </p>
                </div>
              </div>
            ) : (
              <>
                <AppTabs
                  active={activeTab}
                  userCount={userCount}
                  systemCount={systemCount}
                  search={search}
                  onTabChange={setActiveTab}
                  onSearchChange={setSearch}
                  onRefresh={() => void loadApps()}
                  refreshing={loading}
                  refreshDisabled={loading || running || status !== "connected"}
                />
                <div className="min-h-0 flex-1 overflow-y-auto">
                  <AppList
                    apps={filteredApps}
                    selected={selected}
                    onToggle={toggleApp}
                    emptyText={
                      search.trim() ? "Không có ứng dụng nào khớp" : "Chưa có ứng dụng nào"
                    }
                  />
                </div>
              </>
            )}
          </>
        )}
      </main>

      <footer className="border-t border-zinc-800 px-4 py-3">
        <ActionBar
          selectedCount={selected.size}
          running={running}
          canSelectAll={filteredApps.length > 0 && !running}
          onSelectAll={selectAll}
          onUninstall={() => setConfirmMode("uninstall")}
          onRestore={() => setConfirmMode("restore")}
          onClear={() => setSelected(new Set())}
        />
      </footer>

      {confirmMode !== null && (
        <ConfirmDialog
          mode={confirmMode}
          apps={selectedApps}
          onCancel={() => setConfirmMode(null)}
          onConfirm={() => void runBatch(confirmMode)}
        />
      )}
    </div>
  );
}

export default App;
