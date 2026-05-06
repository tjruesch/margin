import { useEffect, useState } from "react";
import {
  deleteAnthropicApiKey,
  hasAnthropicApiKey,
  setAnthropicApiKey,
} from "./file";
import {
  IconChevLeft,
  IconEdit,
  IconHome,
  IconSettings,
  IconSparkle,
} from "./icons";
import type {
  AISettings,
  AudioRetention,
  SummaryModel,
  ThemeSettings,
  WhisperModel,
} from "./settingsStore";
import { loadGlossary } from "./settingsStore";
import { THEMES, darkThemes, getTheme, lightThemes, type Theme } from "./themes";

type Section = "appearance" | "ai" | "editor" | "shortcuts";

export type EditorPrefs = {
  tabSize: number;
  useTabs: boolean;
  softWrap: boolean;
};

type SettingsProps = {
  theme: ThemeSettings;
  ai: AISettings;
  editor: EditorPrefs;
  onThemeChange: (next: ThemeSettings) => void;
  onAIChange: (next: AISettings) => void;
  onEditorChange: (next: EditorPrefs) => void;
  onBack: () => void;
};

const SECTIONS: {
  id: Section;
  label: string;
  icon: React.ReactNode;
}[] = [
  { id: "appearance", label: "Appearance", icon: <IconSettings size={14} sw={1.7} /> },
  { id: "ai", label: "AI", icon: <IconSparkle size={14} sw={1.7} /> },
  { id: "editor", label: "Editor", icon: <IconEdit size={14} sw={1.7} /> },
  { id: "shortcuts", label: "Shortcuts", icon: <IconHome size={14} sw={1.7} /> },
];

const SECTION_TITLE: Record<Section, string> = {
  appearance: "Appearance",
  ai: "AI",
  editor: "Editor",
  shortcuts: "Shortcuts",
};

export function Settings({
  theme,
  ai,
  editor,
  onThemeChange,
  onAIChange,
  onEditorChange,
  onBack,
}: SettingsProps) {
  const [active, setActive] = useState<Section>("appearance");

  const updateTheme = (patch: Partial<ThemeSettings>) =>
    onThemeChange({ ...theme, ...patch });
  const updateAI = (patch: Partial<AISettings>) => onAIChange({ ...ai, ...patch });
  const updateEditor = (patch: Partial<EditorPrefs>) =>
    onEditorChange({ ...editor, ...patch });

  return (
    <div className="home">
      <aside className="home-sidebar" aria-label="Settings sections">
        <div className="home-search-wrap">
          <button type="button" className="home-back-link" onClick={onBack}>
            <IconChevLeft size={14} sw={1.7} />
            <span>Back</span>
          </button>
        </div>
        <nav className="home-nav">
          {SECTIONS.map((s) => (
            <button
              key={s.id}
              type="button"
              className={"home-nav-item" + (active === s.id ? " active" : "")}
              onClick={() => setActive(s.id)}
              aria-current={active === s.id ? "page" : undefined}
            >
              <span className="home-nav-icon">{s.icon}</span>
              <span className="home-nav-label">{s.label}</span>
            </button>
          ))}
        </nav>
      </aside>

      <main className="home-main">
        <header className="home-greeting">
          <div className="home-greeting-text">
            <div className="home-greeting-eyebrow">Settings</div>
            <h1 className="home-greeting-title">{SECTION_TITLE[active]}</h1>
          </div>
        </header>

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
                    onChange={(e) => updateTheme({ syncWithOS: e.target.checked })}
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
                  onSelect={(id) => updateTheme({ lightTheme: id })}
                />
                <ThemePicker
                  groupLabel="Dark"
                  themes={darkThemes()}
                  selectedId={theme.darkTheme}
                  onSelect={(id) => updateTheme({ darkTheme: id })}
                />
              </>
            ) : (
              <ThemePicker
                groupLabel="Theme"
                themes={THEMES}
                selectedId={theme.fixedTheme}
                onSelect={(id) => updateTheme({ fixedTheme: id })}
              />
            )}
          </section>
        )}

        {active === "ai" && <AISection ai={ai} onChange={updateAI} />}

        {active === "editor" && (
          <section className="settings-section">
            <h2>Editor</h2>

            <div className="settings-row settings-row--inline">
              <div className="settings-row-label">Indent</div>
              <div className="settings-row-control">
                <select
                  className="settings-input"
                  value={editor.useTabs ? "tabs" : "spaces"}
                  onChange={(e) => updateEditor({ useTabs: e.target.value === "tabs" })}
                >
                  <option value="spaces">Spaces</option>
                  <option value="tabs">Tabs</option>
                </select>
              </div>
            </div>

            <div className="settings-row settings-row--inline">
              <div className="settings-row-label">Width</div>
              <div className="settings-row-control">
                <select
                  className="settings-input"
                  value={String(editor.tabSize)}
                  onChange={(e) => updateEditor({ tabSize: Number(e.target.value) })}
                >
                  <option value="2">2</option>
                  <option value="4">4</option>
                  <option value="8">8</option>
                </select>
              </div>
            </div>

            <div className="settings-row settings-row--inline">
              <div className="settings-row-label">Wrap lines</div>
              <div className="settings-row-control">
                <label className="toggle">
                  <input
                    type="checkbox"
                    checked={editor.softWrap}
                    onChange={(e) => updateEditor({ softWrap: e.target.checked })}
                  />
                  <span className="toggle-track" aria-hidden="true">
                    <span className="toggle-thumb" />
                  </span>
                </label>
                <span className="settings-hint">
                  {editor.softWrap
                    ? "Long lines wrap to fit the editor width."
                    : "Long lines extend horizontally and the editor scrolls."}
                </span>
              </div>
            </div>
          </section>
        )}

        {active === "shortcuts" && (
          <section className="settings-section">
            <h2>Shortcuts</h2>
            <p className="settings-placeholder">Keyboard shortcut customization coming soon.</p>
          </section>
        )}
        </div>
      </main>
    </div>
  );
}

type AISectionProps = {
  ai: AISettings;
  onChange: (patch: Partial<AISettings>) => void;
};

function AISection({ ai, onChange }: AISectionProps) {
  const [hasKey, setHasKey] = useState<boolean>(false);
  const [draft, setDraft] = useState<string>("");
  const [saving, setSaving] = useState<boolean>(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void hasAnthropicApiKey().then(setHasKey);
  }, []);

  const onSaveKey = async () => {
    const value = draft.trim();
    if (!value) return;
    setSaving(true);
    setError(null);
    try {
      await setAnthropicApiKey(value);
      setDraft("");
      setHasKey(true);
    } catch (e) {
      setError(typeof e === "string" ? e : "Failed to save key");
    } finally {
      setSaving(false);
    }
  };

  const onRemoveKey = async () => {
    if (!confirm("Remove the saved Anthropic API key?")) return;
    setSaving(true);
    setError(null);
    try {
      await deleteAnthropicApiKey();
      setHasKey(false);
    } catch (e) {
      setError(typeof e === "string" ? e : "Failed to remove key");
    } finally {
      setSaving(false);
    }
  };

  return (
    <section className="settings-section">
      <h2>AI</h2>

      <div className="settings-row">
        <div className="settings-row-label">Anthropic API key</div>
        <div className="settings-row-control settings-row-control--col">
          <div className="settings-actions">
            <input
              type="password"
              className="settings-input"
              placeholder={hasKey ? "•••••••• (saved in Keychain)" : "sk-ant-…"}
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              autoComplete="off"
              spellCheck={false}
            />
            <button
              className="ghost"
              onClick={() => void onSaveKey()}
              disabled={saving || !draft.trim()}
            >
              Save
            </button>
            {hasKey && (
              <button className="ghost" onClick={() => void onRemoveKey()} disabled={saving}>
                Remove
              </button>
            )}
          </div>
          <div className="settings-row-control">
            <span className={"settings-status " + (hasKey ? "ok" : "muted")}>
              {hasKey ? "Configured" : "Not configured"}
            </span>
            <span className="settings-hint">
              Stored in macOS Keychain. Required for meeting summarization.
            </span>
          </div>
          {error && <div className="settings-error">{error}</div>}
        </div>
      </div>

      <div className="settings-row settings-row--inline">
        <div className="settings-row-label">Summary model</div>
        <div className="settings-row-control">
          <select
            className="settings-input"
            value={ai.summaryModel}
            onChange={(e) => onChange({ summaryModel: e.target.value as SummaryModel })}
          >
            <option value="claude-sonnet-4-6">Claude Sonnet 4.6 (default — balanced)</option>
            <option value="claude-opus-4-7">Claude Opus 4.7 (most capable, slower)</option>
          </select>
        </div>
      </div>

      <div className="settings-row settings-row--inline">
        <div className="settings-row-label">Transcription model</div>
        <div className="settings-row-control">
          <select
            className="settings-input"
            value={ai.whisperModel}
            onChange={(e) => onChange({ whisperModel: e.target.value as WhisperModel })}
          >
            <option value="large-v3-turbo">large-v3-turbo (~1.6 GB) — recommended</option>
            <option value="large-v3">large-v3 (~3 GB) — slowest, highest accuracy</option>
            <option value="medium">medium (~1.5 GB)</option>
          </select>
          <span className="settings-hint">
            Multilingual; language is auto-detected per recording. Downloaded on first use into
            ~/.margin/models/.
          </span>
        </div>
      </div>

      <div className="settings-row settings-row--inline">
        <div className="settings-row-label">Record system audio</div>
        <div className="settings-row-control">
          <label className="toggle">
            <input
              type="checkbox"
              checked={ai.recordSystemAudio}
              onChange={(e) => onChange({ recordSystemAudio: e.target.checked })}
            />
            <span className="toggle-track" aria-hidden="true">
              <span className="toggle-thumb" />
            </span>
          </label>
          <span className="settings-hint">
            Captures the other side of the call too. Requires screen-recording permission on first
            use.
          </span>
        </div>
      </div>

      <GlossaryRow glossary={ai.glossary} onChange={(next) => onChange({ glossary: next })} />

      <div className="settings-row settings-row--inline">
        <div className="settings-row-label">Keep meeting audio</div>
        <div className="settings-row-control">
          <select
            className="settings-input"
            value={ai.audioRetention}
            onChange={(e) => onChange({ audioRetention: e.target.value as AudioRetention })}
          >
            <option value="forever">Forever</option>
            <option value="30days">30 days</option>
            <option value="7days">7 days</option>
            <option value="never">Never (delete after summarize)</option>
          </select>
        </div>
      </div>
    </section>
  );
}

type GlossaryRowProps = {
  glossary: string[];
  onChange: (next: string[]) => void;
};

function GlossaryRow({ glossary, onChange }: GlossaryRowProps) {
  // Local draft state so users can type partial lines without each keystroke
  // round-tripping through settings + the LazyStore. Commit on blur.
  const [draft, setDraft] = useState<string>(glossary.join("\n"));
  useEffect(() => {
    setDraft(glossary.join("\n"));
  }, [glossary]);

  const commit = () => {
    const next = loadGlossary(draft.split("\n"));
    setDraft(next.join("\n"));
    if (next.length === glossary.length && next.every((t, i) => t === glossary[i])) {
      return;
    }
    onChange(next);
  };

  return (
    <div className="settings-row">
      <div className="settings-row-label">Glossary</div>
      <div className="settings-row-control settings-row-control--col">
        <textarea
          className="settings-input settings-textarea"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          rows={5}
          spellCheck={false}
          placeholder={"emasphere\nELAN\nMargin"}
        />
        <span className="settings-hint">
          One term per line. Used to bias transcription and summarization toward your domain
          vocabulary (product names, jargon, etc.).
        </span>
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

export type { Theme };
export { getTheme };
