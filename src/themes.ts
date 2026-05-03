export type Appearance = "light" | "dark";

export type ThemeVars = {
  // Window chrome
  bg: string;
  bgMuted: string;
  border: string;
  borderMuted: string;
  fg: string;
  fgMuted: string;
  accent: string;
  tabActiveBg: string;
  tabActiveBorder: string;
  // Code blocks
  codeBg: string;
  codeFg: string;
  // Syntax tokens
  hlKeyword: string;
  hlTitle: string;     // function names, classes
  hlAttr: string;      // attributes, numbers, literals
  hlString: string;
  hlBuiltin: string;   // built-in symbols
  hlComment: string;
  hlTag: string;       // selector tags, names, quotes
  hlSection: string;   // headings, sections
};

export type Theme = {
  id: string;
  name: string;
  appearance: Appearance;
  vars: ThemeVars;
};

const githubLight: Theme = {
  id: "github-light",
  name: "GitHub Light",
  appearance: "light",
  vars: {
    bg: "#ffffff",
    bgMuted: "#f6f8fa",
    border: "#d0d7de",
    borderMuted: "#d8dee4",
    fg: "#1f2328",
    fgMuted: "#59636e",
    accent: "#0969da",
    tabActiveBg: "#ffffff",
    tabActiveBorder: "#d0d7de",
    codeBg: "#f6f8fa",
    codeFg: "#24292e",
    hlKeyword: "#d73a49",
    hlTitle: "#6f42c1",
    hlAttr: "#005cc5",
    hlString: "#032f62",
    hlBuiltin: "#e36209",
    hlComment: "#6a737d",
    hlTag: "#22863a",
    hlSection: "#005cc5",
  },
};

const githubDark: Theme = {
  id: "github-dark",
  name: "GitHub Dark",
  appearance: "dark",
  vars: {
    bg: "#0d1117",
    bgMuted: "#161b22",
    border: "#30363d",
    borderMuted: "#21262d",
    fg: "#e6edf3",
    fgMuted: "#8d96a0",
    accent: "#2f81f7",
    tabActiveBg: "#0d1117",
    tabActiveBorder: "#30363d",
    codeBg: "#161b22",
    codeFg: "#c9d1d9",
    hlKeyword: "#ff7b72",
    hlTitle: "#d2a8ff",
    hlAttr: "#79c0ff",
    hlString: "#a5d6ff",
    hlBuiltin: "#ffa657",
    hlComment: "#8b949e",
    hlTag: "#7ee787",
    hlSection: "#1f6feb",
  },
};

const solarizedLight: Theme = {
  id: "solarized-light",
  name: "Solarized Light",
  appearance: "light",
  vars: {
    bg: "#fdf6e3",
    bgMuted: "#eee8d5",
    border: "#93a1a1",
    borderMuted: "#d8d2bf",
    fg: "#586e75",
    fgMuted: "#93a1a1",
    accent: "#268bd2",
    tabActiveBg: "#fdf6e3",
    tabActiveBorder: "#93a1a1",
    codeBg: "#eee8d5",
    codeFg: "#586e75",
    hlKeyword: "#859900",
    hlTitle: "#268bd2",
    hlAttr: "#cb4b16",
    hlString: "#2aa198",
    hlBuiltin: "#b58900",
    hlComment: "#93a1a1",
    hlTag: "#268bd2",
    hlSection: "#d33682",
  },
};

const solarizedDark: Theme = {
  id: "solarized-dark",
  name: "Solarized Dark",
  appearance: "dark",
  vars: {
    bg: "#002b36",
    bgMuted: "#073642",
    border: "#586e75",
    borderMuted: "#0d4150",
    fg: "#93a1a1",
    fgMuted: "#657b83",
    accent: "#268bd2",
    tabActiveBg: "#002b36",
    tabActiveBorder: "#586e75",
    codeBg: "#073642",
    codeFg: "#93a1a1",
    hlKeyword: "#859900",
    hlTitle: "#268bd2",
    hlAttr: "#cb4b16",
    hlString: "#2aa198",
    hlBuiltin: "#b58900",
    hlComment: "#586e75",
    hlTag: "#268bd2",
    hlSection: "#d33682",
  },
};

const dracula: Theme = {
  id: "dracula",
  name: "Dracula",
  appearance: "dark",
  vars: {
    bg: "#282a36",
    bgMuted: "#21222c",
    border: "#44475a",
    borderMuted: "#343746",
    fg: "#f8f8f2",
    fgMuted: "#bbbbbb",
    accent: "#bd93f9",
    tabActiveBg: "#282a36",
    tabActiveBorder: "#44475a",
    codeBg: "#21222c",
    codeFg: "#f8f8f2",
    hlKeyword: "#ff79c6",
    hlTitle: "#50fa7b",
    hlAttr: "#bd93f9",
    hlString: "#f1fa8c",
    hlBuiltin: "#ffb86c",
    hlComment: "#6272a4",
    hlTag: "#8be9fd",
    hlSection: "#bd93f9",
  },
};

const nord: Theme = {
  id: "nord",
  name: "Nord",
  appearance: "dark",
  vars: {
    bg: "#2e3440",
    bgMuted: "#3b4252",
    border: "#434c5e",
    borderMuted: "#3b4252",
    fg: "#eceff4",
    fgMuted: "#d8dee9",
    accent: "#88c0d0",
    tabActiveBg: "#2e3440",
    tabActiveBorder: "#434c5e",
    codeBg: "#3b4252",
    codeFg: "#eceff4",
    hlKeyword: "#81a1c1",
    hlTitle: "#88c0d0",
    hlAttr: "#b48ead",
    hlString: "#a3be8c",
    hlBuiltin: "#d08770",
    hlComment: "#616e88",
    hlTag: "#8fbcbb",
    hlSection: "#5e81ac",
  },
};

export const THEMES: Theme[] = [
  githubLight,
  githubDark,
  solarizedLight,
  solarizedDark,
  dracula,
  nord,
];

export const DEFAULT_LIGHT_THEME_ID = githubLight.id;
export const DEFAULT_DARK_THEME_ID = githubDark.id;

export function getTheme(id: string): Theme | undefined {
  return THEMES.find((t) => t.id === id);
}

export function lightThemes(): Theme[] {
  return THEMES.filter((t) => t.appearance === "light");
}

export function darkThemes(): Theme[] {
  return THEMES.filter((t) => t.appearance === "dark");
}

const VAR_MAP: Record<keyof ThemeVars, string> = {
  bg: "--bg",
  bgMuted: "--bg-muted",
  border: "--border",
  borderMuted: "--border-muted",
  fg: "--fg",
  fgMuted: "--fg-muted",
  accent: "--accent",
  tabActiveBg: "--tab-active-bg",
  tabActiveBorder: "--tab-active-border",
  codeBg: "--code-bg",
  codeFg: "--code-fg",
  hlKeyword: "--hl-keyword",
  hlTitle: "--hl-title",
  hlAttr: "--hl-attr",
  hlString: "--hl-string",
  hlBuiltin: "--hl-builtin",
  hlComment: "--hl-comment",
  hlTag: "--hl-tag",
  hlSection: "--hl-section",
};

/** Apply a theme by writing its vars to :root and setting data-theme=appearance. */
export function applyTheme(theme: Theme): void {
  const root = document.documentElement;
  for (const [key, cssName] of Object.entries(VAR_MAP) as [keyof ThemeVars, string][]) {
    root.style.setProperty(cssName, theme.vars[key]);
  }
  root.dataset.theme = theme.appearance;
  root.dataset.themeId = theme.id;
}
