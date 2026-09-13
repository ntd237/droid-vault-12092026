import type { AppKind } from "../types";

interface AppTabsProps {
  active: AppKind;
  userCount: number;
  systemCount: number;
  search: string;
  onTabChange: (tab: AppKind) => void;
  onSearchChange: (value: string) => void;
  onRefresh: () => void;
  refreshing: boolean;
  refreshDisabled: boolean;
}

const TABS: { key: AppKind; label: string }[] = [
  { key: "User", label: "USER APPS" },
  { key: "System", label: "SYSTEM APPS" },
];

export default function AppTabs({
  active,
  userCount,
  systemCount,
  search,
  onTabChange,
  onSearchChange,
  onRefresh,
  refreshing,
  refreshDisabled,
}: AppTabsProps) {
  const counts: Record<AppKind, number> = { User: userCount, System: systemCount };

  return (
    <div className="border-b border-zinc-800">
      <div className="flex items-center justify-between gap-4 px-2">
        <div className="flex">
          {TABS.map(({ key, label }) => {
            const isActive = key === active;
            return (
              <button
                key={key}
                type="button"
                onClick={() => onTabChange(key)}
                className={`-mb-px border-b-2 px-4 py-3 text-sm font-medium tracking-wide transition-colors ${
                  isActive
                    ? "border-red-600 text-red-400"
                    : "border-transparent text-zinc-400 hover:text-zinc-200"
                }`}
              >
                {label}
                <span
                  className={`ml-2 rounded-full px-1.5 py-0.5 text-[10px] font-normal ${
                    isActive ? "bg-red-600/20 text-red-300" : "bg-zinc-800 text-zinc-400"
                  }`}
                >
                  {counts[key]}
                </span>
              </button>
            );
          })}
        </div>

        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={onRefresh}
            disabled={refreshDisabled}
            className="rounded-lg border border-zinc-700 px-3 py-1.5 text-sm text-zinc-300 transition-colors hover:border-zinc-500 hover:text-zinc-100 disabled:cursor-not-allowed disabled:opacity-40"
          >
            {refreshing ? "Đang làm mới…" : "Làm mới"}
          </button>
          <input
            type="text"
            value={search}
            onChange={(e) => onSearchChange(e.target.value)}
            placeholder="Tìm kiếm theo tên hoặc tên gói…"
            className="w-64 rounded-lg border border-zinc-700 bg-zinc-800/60 px-3 py-1.5 text-sm text-zinc-100 placeholder-zinc-500 outline-none focus:border-red-600/60"
          />
        </div>
      </div>
    </div>
  );
}
