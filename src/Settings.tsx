import { useState } from "react";
import type { Theme } from "./settingsStore";

type Section = "appearance" | "editor" | "shortcuts";

type SettingsProps = {
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
};

const SECTIONS: { id: Section; label: string }[] = [
  { id: "appearance", label: "Appearance" },
  { id: "editor", label: "Editor" },
  { id: "shortcuts", label: "Shortcuts" },
];

const THEME_OPTIONS: { value: Theme; label: string; hint: string }[] = [
  { value: "system", label: "System", hint: "Match the macOS appearance" },
  { value: "light", label: "Light", hint: "Always use the light theme" },
  { value: "dark", label: "Dark", hint: "Always use the dark theme" },
];

export function Settings({ theme, onThemeChange }: SettingsProps) {
  const [active, setActive] = useState<Section>("appearance");

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
            <h2>Appearance</h2>

            <div className="settings-row">
              <div className="settings-row-label">Theme</div>
              <div className="settings-radio-group" role="radiogroup" aria-label="Theme">
                {THEME_OPTIONS.map((opt) => (
                  <label key={opt.value} className="settings-radio">
                    <input
                      type="radio"
                      name="theme"
                      value={opt.value}
                      checked={theme === opt.value}
                      onChange={() => onThemeChange(opt.value)}
                    />
                    <span className="settings-radio-text">
                      <span className="settings-radio-label">{opt.label}</span>
                      <span className="settings-radio-hint">{opt.hint}</span>
                    </span>
                  </label>
                ))}
              </div>
            </div>
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
