import { LazyStore } from "@tauri-apps/plugin-store";

export type Theme = "system" | "light" | "dark";

export type AppSettings = {
  theme: Theme;
};

export const DEFAULT_SETTINGS: AppSettings = {
  theme: "system",
};

const STORE_FILE = "settings.json";

let store: LazyStore | null = null;

function getStore(): LazyStore {
  if (!store) {
    store = new LazyStore(STORE_FILE);
  }
  return store;
}

function isTheme(value: unknown): value is Theme {
  return value === "system" || value === "light" || value === "dark";
}

export async function loadSettings(): Promise<AppSettings> {
  const s = getStore();
  const theme = await s.get<Theme>("theme");
  return {
    theme: isTheme(theme) ? theme : DEFAULT_SETTINGS.theme,
  };
}

export async function saveSetting<K extends keyof AppSettings>(
  key: K,
  value: AppSettings[K],
): Promise<void> {
  const s = getStore();
  await s.set(key, value);
  await s.save();
}
