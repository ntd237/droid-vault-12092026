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

  return (
    <ul className="divide-y divide-zinc-800/60">
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
