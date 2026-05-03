import { useState } from "react";
import type { ThemeSettings } from "./settingsStore";
import { THEMES, darkThemes, getTheme, lightThemes, type Theme } from "./themes";

type Section = "appearance" | "editor" | "shortcuts";

type SettingsProps = {
  theme: ThemeSettings;
  onThemeChange: (next: ThemeSettings) => void;
};

const SECTIONS: { id: Section; label: string }[] = [
  { id: "appearance", label: "Appearance" },
  { id: "editor", label: "Editor" },
  { id: "shortcuts", label: "Shortcuts" },
];

export function Settings({ theme, onThemeChange }: SettingsProps) {
  const [active, setActive] = useState<Section>("appearance");

  const update = (patch: Partial<ThemeSettings>) =>
    onThemeChange({ ...theme, ...patch });

  return (
    <div className="settings">
      <nav className="settings-nav" aria-label="Settings sections">
        {SECTIONS.map((s) => (
          <button
            key={s.id}
            className={"settings-nav-item " + (active === s.id ? "active" : "")}
            onClick={() => setActive(s.id)}
            aria-current={active === s.id ? "page" : undefined}
          >
            {s.label}
          </button>
        ))}
      </nav>

      <div className="settings-content">
        {active === "appearance" && (
          <section className="settings-section">
            <h2>Themes</h2>

            <div className="settings-row settings-row--inline">
              <div className="settings-row-label">Sync with OS</div>
              <div className="settings-row-control">
                <label className="toggle">
                  <input
                    type="checkbox"
                    checked={theme.syncWithOS}
                    onChange={(e) => update({ syncWithOS: e.target.checked })}
                  />
                  <span className="toggle-track" aria-hidden="true">
                    <span className="toggle-thumb" />
                  </span>
                </label>
                <span className="settings-hint">
                  {theme.syncWithOS
                    ? "Automatically switch between light and dark themes when your system does."
                    : "Use one fixed theme regardless of system appearance."}
                </span>
              </div>
            </div>

            {theme.syncWithOS ? (
              <>
                <ThemePicker
                  groupLabel="Light"
                  themes={lightThemes()}
                  selectedId={theme.lightTheme}
                  onSelect={(id) => update({ lightTheme: id })}
                />
                <ThemePicker
                  groupLabel="Dark"
                  themes={darkThemes()}
                  selectedId={theme.darkTheme}
                  onSelect={(id) => update({ darkTheme: id })}
                />
              </>
            ) : (
              <ThemePicker
                groupLabel="Theme"
                themes={THEMES}
                selectedId={theme.fixedTheme}
                onSelect={(id) => update({ fixedTheme: id })}
              />
            )}
          </section>
        )}

        {active === "editor" && (
          <section className="settings-section">
            <h2>Editor</h2>
            <p className="settings-placeholder">Editor preferences coming soon.</p>
          </section>
        )}

        {active === "shortcuts" && (
          <section className="settings-section">
            <h2>Shortcuts</h2>
            <p className="settings-placeholder">Keyboard shortcut customization coming soon.</p>
          </section>
        )}
      </div>
    </div>
  );
}

type PickerProps = {
  groupLabel: string;
  themes: Theme[];
  selectedId: string;
  onSelect: (id: string) => void;
};

function ThemePicker({ groupLabel, themes, selectedId, onSelect }: PickerProps) {
  return (
    <div className="settings-row settings-row--inline">
      <div className="settings-row-label">{groupLabel}</div>
      <div className="theme-grid" role="radiogroup" aria-label={`${groupLabel} theme`}>
        {themes.map((t) => (
          <ThemeCard
            key={t.id}
            theme={t}
            selected={t.id === selectedId}
            onSelect={() => onSelect(t.id)}
          />
        ))}
      </div>
    </div>
  );
}

function ThemeCard({
  theme,
  selected,
  onSelect,
}: {
  theme: Theme;
  selected: boolean;
  onSelect: () => void;
}) {
  // Build inline styles from theme vars so the swatch shows the theme even before it's applied globally.
  const swatch: React.CSSProperties = {
    background: theme.vars.bg,
    color: theme.vars.fg,
    borderColor: theme.vars.border,
  };
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      className={"theme-card " + (selected ? "selected" : "")}
      onClick={onSelect}
    >
      <span className="theme-card-preview" style={swatch}>
        <span className="theme-card-line" style={{ background: theme.vars.fg, opacity: 0.85 }} />
        <span className="theme-card-line short" style={{ background: theme.vars.hlKeyword }} />
        <span className="theme-card-line" style={{ background: theme.vars.hlString }} />
        <span className="theme-card-line short" style={{ background: theme.vars.hlComment }} />
      </span>
      <span className="theme-card-name">{theme.name}</span>
    </button>
  );
}

// Re-export so consumers don't need both modules
export type { Theme };
export { getTheme };
