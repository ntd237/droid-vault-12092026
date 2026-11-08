import type { AppInfo } from "../types";
import AppRow from "./AppRow";

interface AppListProps {
  apps: AppInfo[];
  selected: Set<string>;
  onToggle: (pkg: string) => void;
  emptyText: string;
}

export default function AppList({ apps, selected, onToggle, emptyText }: AppListProps) {
  if (apps.length === 0) {
    return (
      <div className="flex h-full items-center justify-center p-4">
        <p className="text-zinc-500">{emptyText}</p>
      </div>
    );
  }

  // Grid auto-placement fills row-major (left to right, then next row), so the
  // A-Z order keeps reading top rows before lower rows in every layout.
  // Columns scale with window size: 1 (<lg) -> 2 (>=lg/1024px, the window's
  // minWidth, so two snug columns are always on screen) -> 3 (2xl). Rows keep
  // full name/package text via truncate only as a rare-overflow guard.
  return (
    <ul className="grid grid-cols-1 items-start gap-x-6 lg:grid-cols-2 2xl:grid-cols-3">
      {apps.map((app) => (
        <AppRow
          key={app.package}
          app={app}
          selected={selected.has(app.package)}
          onToggle={onToggle}
        />
      ))}
    </ul>
  );
}
