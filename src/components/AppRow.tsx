import type { AppInfo } from "../types";

interface AppRowProps {
  app: AppInfo;
  selected: boolean;
  onToggle: (pkg: string) => void;
}

export default function AppRow({ app, selected, onToggle }: AppRowProps) {
  const iconSrc = app.icon_base64
    ? `data:image/png;base64,${app.icon_base64}`
    : null;

  return (
    <li>
      <button
        type="button"
        onClick={() => onToggle(app.package)}
        className={`flex w-full items-center gap-3 px-3 py-2 text-left transition-colors ${
          selected ? "bg-red-500/10" : "hover:bg-zinc-800/60"
        } ${!app.installed ? "opacity-45" : "opacity-100"}`}
      >
        <span
          aria-hidden
          className={`flex h-4 w-4 shrink-0 items-center justify-center rounded border ${
            selected ? "border-red-500 bg-red-600" : "border-zinc-600"
          }`}
        >
          {selected && (
            <svg viewBox="0 0 12 12" className="h-3 w-3 text-white" fill="none">
              <path
                d="M2.5 6.5 5 9l4.5-6"
                stroke="currentColor"
                strokeWidth="1.5"
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            </svg>
          )}
        </span>

        {iconSrc ? (
          <img
            src={iconSrc}
            alt=""
            className="h-10 w-10 shrink-0 rounded-lg bg-zinc-800 object-contain p-0.5"
          />
        ) : (
          <span className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg bg-zinc-700 text-sm font-semibold text-zinc-300">
            {app.label.charAt(0).toUpperCase()}
          </span>
        )}

        <span className="min-w-0 flex-1">
          <span className="block truncate text-sm font-medium text-zinc-100">
            {app.label}
          </span>
          <span className="block truncate font-mono text-xs text-zinc-500">
            {app.package}
          </span>
        </span>

        {!app.installed && (
          <span
            className={`shrink-0 rounded-md border px-2 py-0.5 text-xs ${
              app.restorable
                ? "border-amber-500/40 bg-amber-500/10 text-amber-300"
                : "border-zinc-600/60 bg-zinc-700/30 text-zinc-400"
            }`}
          >
            {app.restorable ? "Đã gỡ (khôi phục được)" : "Đã gỡ"}
          </span>
        )}
      </button>
    </li>
  );
}
