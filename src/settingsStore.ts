import { LazyStore } from "@tauri-apps/plugin-store";
import {
  DEFAULT_DARK_THEME_ID,
  DEFAULT_LIGHT_THEME_ID,
  THEMES,
  getTheme,
} from "./themes";

export type ThemeSettings = {
  syncWithOS: boolean;
  fixedTheme: string;
  lightTheme: string;
  darkTheme: string;
};

export type SummaryModel = "claude-sonnet-4-6" | "claude-opus-4-7";
export type WhisperModel = "medium" | "large-v3-turbo" | "large-v3";
export type AudioRetention = "forever" | "30days" | "7days" | "never";

export type AISettings = {
  summaryModel: SummaryModel;
  whisperModel: WhisperModel;
  recordSystemAudio: boolean;
  audioRetention: AudioRetention;
  glossary: string[];
};

export type AppSettings = {
  theme: ThemeSettings;
  ai: AISettings;
  recentFiles: string[];
};

const RECENT_FILES_LIMIT = 20;

export const DEFAULT_AI_SETTINGS: AISettings = {
  summaryModel: "claude-sonnet-4-6",
  whisperModel: "large-v3-turbo",
  recordSystemAudio: true,
  audioRetention: "forever",
  glossary: [],
};

const GLOSSARY_MAX_ENTRIES = 64;
const GLOSSARY_MAX_TERM_LEN = 64;

export const DEFAULT_SETTINGS: AppSettings = {
  theme: {
    syncWithOS: true,
    fixedTheme: DEFAULT_LIGHT_THEME_ID,
    lightTheme: DEFAULT_LIGHT_THEME_ID,
    darkTheme: DEFAULT_DARK_THEME_ID,
  },
  ai: DEFAULT_AI_SETTINGS,
  recentFiles: [],
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

const SUMMARY_MODELS: SummaryModel[] = ["claude-sonnet-4-6", "claude-opus-4-7"];
const WHISPER_MODELS: WhisperModel[] = ["medium", "large-v3-turbo", "large-v3"];
const RETENTIONS: AudioRetention[] = ["forever", "30days", "7days", "never"];

function pick<T extends string>(allowed: readonly T[], value: unknown, fallback: T): T {
  return (allowed as readonly string[]).includes(value as string) ? (value as T) : fallback;
}

function loadTheme(raw: unknown): ThemeSettings {
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
      syncWithOS:
        typeof obj.syncWithOS === "boolean"
          ? obj.syncWithOS
          : DEFAULT_SETTINGS.theme.syncWithOS,
      fixedTheme: fixed,
      lightTheme: light,
      darkTheme: dark,
    };
  }
  // Legacy schema: a single string "system" | "light" | "dark"
  if (raw === "system") return { ...DEFAULT_SETTINGS.theme, syncWithOS: true };
  if (raw === "light")
    return { ...DEFAULT_SETTINGS.theme, syncWithOS: false, fixedTheme: DEFAULT_LIGHT_THEME_ID };
  if (raw === "dark")
    return { ...DEFAULT_SETTINGS.theme, syncWithOS: false, fixedTheme: DEFAULT_DARK_THEME_ID };
  return DEFAULT_SETTINGS.theme;
}

function loadAI(raw: unknown): AISettings {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return DEFAULT_AI_SETTINGS;
  const obj = raw as Record<string, unknown>;
  return {
    summaryModel: pick(SUMMARY_MODELS, obj.summaryModel, DEFAULT_AI_SETTINGS.summaryModel),
    whisperModel: pick(WHISPER_MODELS, obj.whisperModel, DEFAULT_AI_SETTINGS.whisperModel),
    recordSystemAudio:
      typeof obj.recordSystemAudio === "boolean"
        ? obj.recordSystemAudio
        : DEFAULT_AI_SETTINGS.recordSystemAudio,
    audioRetention: pick(RETENTIONS, obj.audioRetention, DEFAULT_AI_SETTINGS.audioRetention),
    glossary: loadGlossary(obj.glossary),
  };
}

export function loadGlossary(raw: unknown): string[] {
  if (!Array.isArray(raw)) return [];
  const seen = new Set<string>();
  const out: string[] = [];
  for (const entry of raw) {
    if (typeof entry !== "string") continue;
    const trimmed = entry.trim();
    if (!trimmed || trimmed.length > GLOSSARY_MAX_TERM_LEN) continue;
    const key = trimmed.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(trimmed);
    if (out.length >= GLOSSARY_MAX_ENTRIES) break;
  }
  return out;
}

function loadRecentFiles(raw: unknown): string[] {
  if (!Array.isArray(raw)) return [];
  return raw
    .filter((p): p is string => typeof p === "string" && p.length > 0)
    .slice(0, RECENT_FILES_LIMIT);
}

export async function loadSettings(): Promise<AppSettings> {
  const s = getStore();
  const [rawTheme, rawAI, rawRecent] = await Promise.all([
    s.get<unknown>("theme"),
    s.get<unknown>("ai"),
    s.get<unknown>("recentFiles"),
  ]);
  return {
    theme: loadTheme(rawTheme),
    ai: loadAI(rawAI),
    recentFiles: loadRecentFiles(rawRecent),
  };
}

export async function saveTheme(theme: ThemeSettings): Promise<void> {
  const s = getStore();
  await s.set("theme", theme);
  await s.save();
}

export async function saveAI(ai: AISettings): Promise<void> {
  const s = getStore();
  await s.set("ai", ai);
  await s.save();
}

/// Push `path` to the front of recents (dedup, cap at RECENT_FILES_LIMIT).
/// Returns the new list so the caller can update React state in lockstep.
export async function addRecentFile(path: string, current: string[]): Promise<string[]> {
  const next = [path, ...current.filter((p) => p !== path)].slice(0, RECENT_FILES_LIMIT);
  const s = getStore();
  await s.set("recentFiles", next);
  await s.save();
  return next;
}
