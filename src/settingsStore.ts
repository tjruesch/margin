import { LazyStore } from "@tauri-apps/plugin-store";
import {
  DEFAULT_DARK_THEME_ID,
  DEFAULT_LIGHT_THEME_ID,
  THEMES,
  getTheme,
} from "./themes";

export type ThemeSettings = {
  syncWithOS: boolean;
  fixedTheme: string; // theme id used when syncWithOS = false
  lightTheme: string; // theme id used in light system mode (when syncWithOS = true)
  darkTheme: string; // theme id used in dark system mode (when syncWithOS = true)
};

export type AppSettings = {
  theme: ThemeSettings;
};

export const DEFAULT_SETTINGS: AppSettings = {
  theme: {
    syncWithOS: true,
    fixedTheme: DEFAULT_LIGHT_THEME_ID,
    lightTheme: DEFAULT_LIGHT_THEME_ID,
    darkTheme: DEFAULT_DARK_THEME_ID,
  },
};

const STORE_FILE = "settings.json";

let store: LazyStore | null = null;

function getStore(): LazyStore {
  if (!store) {
    store = new LazyStore(STORE_FILE);
  }
  return store;
}

function isThemeId(value: unknown): value is string {
  return typeof value === "string" && THEMES.some((t) => t.id === value);
}

function isLightThemeId(id: string): boolean {
  return getTheme(id)?.appearance === "light";
}

function isDarkThemeId(id: string): boolean {
  return getTheme(id)?.appearance === "dark";
}

export async function loadSettings(): Promise<AppSettings> {
  const s = getStore();
  const raw = await s.get<unknown>("theme");

  // New schema: an object with the four keys
  if (raw && typeof raw === "object" && !Array.isArray(raw)) {
    const obj = raw as Record<string, unknown>;
    const fixed = isThemeId(obj.fixedTheme) ? obj.fixedTheme : DEFAULT_SETTINGS.theme.fixedTheme;
    const light =
      isThemeId(obj.lightTheme) && isLightThemeId(obj.lightTheme)
        ? obj.lightTheme
        : DEFAULT_SETTINGS.theme.lightTheme;
    const dark =
      isThemeId(obj.darkTheme) && isDarkThemeId(obj.darkTheme)
        ? obj.darkTheme
        : DEFAULT_SETTINGS.theme.darkTheme;
    return {
      theme: {
        syncWithOS:
          typeof obj.syncWithOS === "boolean"
            ? obj.syncWithOS
            : DEFAULT_SETTINGS.theme.syncWithOS,
        fixedTheme: fixed,
        lightTheme: light,
        darkTheme: dark,
      },
    };
  }

  // Legacy schema: a single string "system" | "light" | "dark"
  if (raw === "system") {
    return { theme: { ...DEFAULT_SETTINGS.theme, syncWithOS: true } };
  }
  if (raw === "light") {
    return {
      theme: {
        ...DEFAULT_SETTINGS.theme,
        syncWithOS: false,
        fixedTheme: DEFAULT_LIGHT_THEME_ID,
      },
    };
  }
  if (raw === "dark") {
    return {
      theme: {
        ...DEFAULT_SETTINGS.theme,
        syncWithOS: false,
        fixedTheme: DEFAULT_DARK_THEME_ID,
      },
    };
  }

  return DEFAULT_SETTINGS;
}

export async function saveTheme(theme: ThemeSettings): Promise<void> {
  const s = getStore();
  await s.set("theme", theme);
  await s.save();
}
