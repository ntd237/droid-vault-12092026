import { useCallback, useEffect, useState } from "react";
import { getConfig, saveTheme } from "../lib/tauri";

export type Theme = "light" | "dark";

const THEME_CLASS = "dark";

// Light is the default: the "dark" class is only ever added, so first paint
// (before config loads) is always light with no flash.
export function useTheme() {
  const [theme, setTheme] = useState<Theme>("light");

  useEffect(() => {
    let cancelled = false;
    getConfig()
      .then((config) => {
        if (!cancelled && (config.theme === "light" || config.theme === "dark")) {
          setTheme(config.theme);
        }
      })
      .catch(() => {
        // Keep the light default when config is unavailable.
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    document.documentElement.classList.toggle(THEME_CLASS, theme === "dark");
  }, [theme]);

  const toggleTheme = useCallback(() => {
    setTheme((prev) => {
      const next: Theme = prev === "light" ? "dark" : "light";
      void saveTheme(next).catch(() => {
        // Persist best-effort; the UI keeps the toggled value either way.
      });
      return next;
    });
  }, []);

  return { theme, toggleTheme };
}
