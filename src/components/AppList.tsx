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
  // A-Z order keeps reading top rows before lower rows in both 1- and 2-column
  // layouts. `lg` breakpoint: small window = 1 column, large window = 2.
  return (
    <ul className="grid grid-cols-1 items-start gap-x-6 lg:grid-cols-2">
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
