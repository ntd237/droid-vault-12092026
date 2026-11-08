import type { AppKind, InstallFilter } from "../types";

interface AppTabsProps {
  active: AppKind;
  userCount: number;
  systemCount: number;
  search: string;
  filter: InstallFilter;
  onTabChange: (tab: AppKind) => void;
  onSearchChange: (value: string) => void;
  onFilterChange: (filter: InstallFilter) => void;
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
  filter,
  onTabChange,
  onSearchChange,
  onFilterChange,
  onRefresh,
  refreshing,
  refreshDisabled,
}: AppTabsProps) {
  const counts: Record<AppKind, number> = { User: userCount, System: systemCount };

  return (
    <div className="border-b border-zinc-200 dark:border-zinc-800">
      <div className="flex items-center justify-between gap-4 px-2">
        <div className="flex">
          {TABS.map(({ key, label }) => {
            const isActive = key === active;
            return (
              <button
                key={key}
                type="button"
                onClick={() => onTabChange(key)}
                className={`-mb-px whitespace-nowrap border-b-2 px-4 py-3 text-sm font-medium tracking-wide transition-colors ${
                  isActive
                    ? "border-red-600 text-red-600 dark:text-red-400"
                    : "border-transparent text-zinc-500 hover:text-zinc-800 dark:text-zinc-400 dark:hover:text-zinc-200"
                }`}
              >
                {label}
                <span
                  className={`ml-2 rounded-full px-1.5 py-0.5 text-[10px] font-normal ${
                    isActive
                      ? "bg-red-600/20 text-red-600 dark:text-red-300"
                      : "bg-zinc-200 text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400"
                  }`}
                >
                  {counts[key]}
                </span>
              </button>
            );
          })}
        </div>

        <div className="flex shrink-0 items-center gap-2">
          <div className="relative flex shrink-0 items-center">
            <svg
              viewBox="0 0 16 16"
              aria-hidden
              className="pointer-events-none absolute left-2 h-3.5 w-3.5 text-zinc-500 dark:text-zinc-400"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.5"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="M1.5 2.5h13L9.5 8.5v5l-3-1.5v-3.5l-5-6Z" />
            </svg>
            <select
              value={filter}
              onChange={(e) => onFilterChange(e.target.value as InstallFilter)}
              aria-label="Lọc theo trạng thái cài đặt"
              className="appearance-none rounded-lg border border-zinc-300 bg-white py-1.5 pl-7 pr-6 text-sm text-zinc-700 outline-none focus:border-red-600/60 dark:border-zinc-700 dark:bg-zinc-800/60 dark:text-zinc-300"
            >
              <option value="all">Tất cả</option>
              <option value="installed">Đang cài đặt</option>
              <option value="uninstalled">Đã gỡ</option>
            </select>
            <svg
              viewBox="0 0 12 12"
              aria-hidden
              className="pointer-events-none absolute right-1.5 h-3 w-3 text-zinc-500 dark:text-zinc-400"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.5"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="M2.5 4.5 6 8l3.5-3.5" />
            </svg>
          </div>
          <input
            type="text"
            value={search}
            onChange={(e) => onSearchChange(e.target.value)}
            placeholder="Tìm kiếm theo tên hoặc tên gói…"
            className="w-60 rounded-lg border border-zinc-300 bg-white px-3 py-1.5 text-sm text-zinc-900 placeholder-zinc-400 outline-none focus:border-red-600/60 dark:border-zinc-700 dark:bg-zinc-800/60 dark:text-zinc-100 dark:placeholder-zinc-500"
          />
          <button
            type="button"
            onClick={onRefresh}
            disabled={refreshDisabled}
            title={refreshing ? "Đang làm mới…" : "Làm mới"}
            aria-label="Làm mới"
            className="flex h-[34px] w-[34px] shrink-0 items-center justify-center rounded-lg border border-zinc-300 text-zinc-600 transition-colors hover:border-zinc-500 hover:text-zinc-900 disabled:cursor-not-allowed disabled:opacity-40 dark:border-zinc-700 dark:text-zinc-300 dark:hover:text-zinc-100"
          >
            {refreshing ? (
              <span
                aria-hidden
                className="inline-block h-3.5 w-3.5 animate-spin rounded-full border-2 border-zinc-400 border-t-transparent dark:border-zinc-500"
              />
            ) : (
              <svg
                viewBox="0 0 24 24"
                aria-hidden
                className="h-4 w-4"
                fill="none"
                stroke="currentColor"
                strokeWidth="1.8"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <path d="M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8" />
                <path d="M21 3v5h-5" />
              </svg>
            )}
          </button>
        </div>
      </div>
    </div>
  );
}
