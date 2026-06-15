import { useEffect, useState } from "react";
import { ask, open as openDialog } from "@tauri-apps/plugin-dialog";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  type ConnectorInfo,
  type ConnectorStatusEvent,
  connectGithub,
  deleteAnthropicApiKey,
  deleteConnector,
  deleteFirecrawlApiKey,
  deleteVoyageApiKey,
  exportNotes,
  forceReindexEmbeddings,
  hasAnthropicApiKey,
  hasFirecrawlApiKey,
  hasVoyageApiKey,
  listConnectors,
  listOAuthProviders,
  type OAuthProviderInfo,
  setAnthropicApiKey,
  setFirecrawlApiKey,
  setVoyageApiKey,
  startOAuthConnector,
  syncConnectorNow,
  type ChatTurnMetric,
  listChatTurnMetrics,
} from "./file";
import { PromptInspector } from "./PromptInspector";
import type { ChatMessageView } from "./ChatMessage";
import {
  IconBrand,
  IconChevLeft,
  IconEdit,
  IconHome,
  IconLink,
  IconSearch,
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

type Section =
  | "appearance"
  | "ai"
  | "connectors"
  | "editor"
  | "shortcuts"
  | "data"
  | "diagnostics";

export type EditorPrefs = {
  tabSize: number;
  useTabs: boolean;
  softWrap: boolean;
  fontSize: number;
};

const FONT_SIZES = [12, 13, 14, 15, 16, 18, 20];

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
  { id: "connectors", label: "Connectors", icon: <IconLink size={14} sw={1.7} /> },
  { id: "editor", label: "Editor", icon: <IconEdit size={14} sw={1.7} /> },
  { id: "shortcuts", label: "Shortcuts", icon: <IconHome size={14} sw={1.7} /> },
  { id: "data", label: "Data", icon: <IconSettings size={14} sw={1.7} /> },
  { id: "diagnostics", label: "Diagnostics", icon: <IconSearch size={14} sw={1.7} /> },
];

const SECTION_TITLE: Record<Section, string> = {
  appearance: "Appearance",
  ai: "AI",
  connectors: "Connectors",
  editor: "Editor",
  shortcuts: "Shortcuts",
  data: "Data",
  diagnostics: "Diagnostics",
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
        <div className="home-titlebar" data-tauri-drag-region />
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
        <div className="home-main-titlebar" data-tauri-drag-region />
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
              <div className="settings-row-label">Font size</div>
              <div className="settings-row-control">
                <select
                  className="settings-input"
                  value={String(editor.fontSize)}
                  onChange={(e) => updateEditor({ fontSize: Number(e.target.value) })}
                >
                  {FONT_SIZES.map((s) => (
                    <option key={s} value={String(s)}>
                      {s}px
                    </option>
                  ))}
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

        {active === "connectors" && <ConnectorsSection />}

        {active === "shortcuts" && (
          <section className="settings-section">
            <h2>Shortcuts</h2>
            <p className="settings-placeholder">Keyboard shortcut customization coming soon.</p>
          </section>
        )}

        {active === "data" && <DataSection />}

        {active === "diagnostics" && <DiagnosticsSection />}
        </div>
      </main>
    </div>
  );
}

/// Settings → Data. After #112 every note lives in SQLite; this
/// section is the user-facing escape hatch — dump every row to disk
/// as `<bundle_id>/note.md` files with frontmatter, round-trippable
/// through the legacy reader.
function DataSection() {
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);

  const onExport = async () => {
    setStatus(null);
    let target: string | null = null;
    try {
      const picked = await openDialog({ directory: true, multiple: false });
      if (typeof picked === "string") target = picked;
    } catch (e) {
      console.error("[settings] export dir picker failed:", e);
      return;
    }
    if (!target) return;
    setBusy(true);
    try {
      const written = await exportNotes(target);
      setStatus(`Exported ${written} note${written === 1 ? "" : "s"}.`);
    } catch (e) {
      console.error("[settings] exportNotes failed:", e);
      setStatus(`Export failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="settings-section">
      <h2>Data</h2>
      <p className="settings-section-desc">
        Notes live in Margin&apos;s local database. Export anytime to get a
        folder of plain Markdown files you can read in any editor, sync
        through iCloud or Dropbox, or keep as a backup.
      </p>
      <div className="settings-row">
        <button
          type="button"
          className="settings-btn"
          onClick={onExport}
          disabled={busy}
        >
          {busy ? "Exporting…" : "Export all notes…"}
        </button>
      </div>
      {status && <p className="settings-helper">{status}</p>}
    </section>
  );
}

/// Settings → Connectors. The backend `SyncRunner` emits
/// `connector-status` whenever any registered connector syncs (or is
/// added / removed via OAuth flow); we refetch on each event so the
/// per-row status stays live without polling.
function ConnectorsSection() {
  const [connectors, setConnectors] = useState<ConnectorInfo[]>([]);
  const [providers, setProviders] = useState<OAuthProviderInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [pickerOpen, setPickerOpen] = useState(false);

  const refresh = async () => {
    try {
      const [list, provs] = await Promise.all([listConnectors(), listOAuthProviders()]);
      setConnectors(list);
      setProviders(provs);
    } catch (e) {
      console.error("[settings] listConnectors failed:", e);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void refresh();
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    (async () => {
      const fn = await listen<ConnectorStatusEvent>("connector-status", () => {
        void refresh();
      });
      if (cancelled) {
        fn();
      } else {
        unlisten = fn;
      }
    })();
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, []);

  const onRemove = async (connectorId: string, displayName: string) => {
    const confirmed = await ask(
      `Remove ${displayName}? Margin's local copy of its data will be cleared. The connection in your account stays granted until you revoke it on the provider's side.`,
      { title: "Remove connector", kind: "warning" },
    );
    if (!confirmed) return;
    try {
      await deleteConnector(connectorId);
    } catch (e) {
      console.error("[settings] deleteConnector failed:", e);
    }
  };

  const onReconnect = async (kind: string) => {
    try {
      await startOAuthConnector(kind);
    } catch (e) {
      console.error("[settings] reconnect failed:", e);
    }
  };

  const onSyncNow = async (connectorId: string) => {
    try {
      await syncConnectorNow(connectorId);
    } catch (e) {
      console.error("[settings] syncConnectorNow failed:", e);
    }
  };

  const noProviders = providers.length === 0;

  return (
    <section className="settings-section">
      <div className="settings-section-header">
        <h2>Connectors</h2>
        <button
          type="button"
          className="settings-add-btn"
          onClick={() => setPickerOpen(true)}
          disabled={noProviders}
          title={
            noProviders
              ? "No OAuth client IDs configured at build time"
              : "Connect an external account"
          }
        >
          + Add connector
        </button>
      </div>
      <p className="settings-section-intro">
        Pull signals from external systems — calendar, email, chat — into your
        notes context. Connectors authenticate via OAuth and Margin only
        stores tokens locally in your keychain.
      </p>
      {loading ? (
        <p className="settings-placeholder">Loading…</p>
      ) : connectors.length === 0 ? (
        <div className="settings-empty">
          <div className="settings-empty-title">No connectors configured</div>
          <div className="settings-empty-body">
            {noProviders
              ? "No OAuth providers built into this binary. See the README for setup."
              : "Click + Add connector to authenticate one of the supported providers."}
          </div>
        </div>
      ) : (
        <div className="connector-list">
          {connectors.map((c) => (
            <ConnectorRow
              key={c.id}
              info={c}
              onRemove={() => onRemove(c.id, c.display_name)}
              onReconnect={() => onReconnect(c.kind)}
              onSyncNow={() => onSyncNow(c.id)}
            />
          ))}
        </div>
      )}
      <GitHubConnectCard
        connected={connectors.some((c) => c.kind === "github")}
        onConnected={() => void refresh()}
      />
      {pickerOpen && (
        <ConnectorPickerModal
          providers={providers}
          onClose={() => setPickerOpen(false)}
        />
      )}
    </section>
  );
}

/// GitHub connects via a personal access token rather than OAuth (no
/// build-time client id needed), so it gets its own card below the OAuth
/// connectors. Once connected it also shows up as a regular connector
/// row above — this card stays for rotating the token.
function GitHubConnectCard({
  connected,
  onConnected,
}: {
  connected: boolean;
  onConnected: () => void;
}) {
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const onConnect = async () => {
    const value = token.trim();
    if (!value) return;
    setBusy(true);
    setError(null);
    try {
      await connectGithub(value);
      setToken("");
      onConnected();
    } catch (e) {
      setError(
        typeof e === "string"
          ? e
          : e instanceof Error
          ? e.message
          : "Failed to connect GitHub",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="github-card">
      <div className="github-card-head">
        <IconBrand kind="github" size={16} />
        <span className="github-card-title">GitHub</span>
        {connected && <span className="github-card-badge">Connected</span>}
      </div>
      <p className="settings-section-intro">
        {connected
          ? "Connected. Paste a new token to rotate it — or manage / remove the connector above."
          : "Build a changelog from your GitHub activity: merged pull requests as delivered features, commits as work in progress. Polled every 15 minutes; the last 30 days are backfilled on connect."}
      </p>
      <div className="settings-actions">
        <input
          type="password"
          className="settings-input"
          placeholder="ghp_… or github_pat_…"
          value={token}
          onChange={(e) => setToken(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") void onConnect();
          }}
          autoComplete="off"
          spellCheck={false}
        />
        <button
          className="ghost"
          onClick={() => void onConnect()}
          disabled={busy || !token.trim()}
        >
          {busy ? "Connecting…" : connected ? "Update token" : "Connect"}
        </button>
      </div>
      {error && <div className="settings-error">{error}</div>}
      <span className="settings-hint">
        Create a token at GitHub → Settings → Developer settings → Personal
        access tokens. Classic tokens need the <code>repo</code> scope (or{" "}
        <code>public_repo</code> for public repos only). Stored only in your
        macOS keychain.
      </span>
    </div>
  );
}

function ConnectorRow({
  info,
  onRemove,
  onReconnect,
  onSyncNow,
}: {
  info: ConnectorInfo;
  onRemove: () => void;
  onReconnect: () => void;
  onSyncNow: () => void;
}) {
  const status = info.last_error
    ? "error"
    : info.last_success_ms
    ? "ok"
    : "pending";
  const lastLabel = info.last_sync_ms
    ? new Date(info.last_sync_ms).toLocaleString()
    : "never";
  const reauthNeeded = info.last_error?.startsWith("reauth_needed:");
  return (
    <div className={`connector-row connector-row-${status}`}>
      <div className="connector-row-body">
        <div className="connector-row-title">{info.display_name}</div>
        <div className="connector-row-sub">
          <span className="connector-row-kind">{info.kind}</span>
          <span className="connector-row-sep">·</span>
          <span className="connector-row-last">last sync: {lastLabel}</span>
        </div>
        {info.last_error && (
          <div className="connector-row-error">{info.last_error}</div>
        )}
      </div>
      <div className="connector-row-actions">
        {reauthNeeded && (
          <button
            type="button"
            className="connector-row-reconnect"
            onClick={onReconnect}
          >
            Reconnect
          </button>
        )}
        <button
          type="button"
          className="connector-row-sync"
          onClick={onSyncNow}
          title="Trigger a sync on the next tick (within 15s)"
        >
          Sync now
        </button>
        <button
          type="button"
          className="connector-row-remove"
          onClick={onRemove}
        >
          Remove
        </button>
      </div>
    </div>
  );
}

function ConnectorPickerModal({
  providers,
  onClose,
}: {
  providers: OAuthProviderInfo[];
  onClose: () => void;
}) {
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const onPick = async (kind: string) => {
    setPending(kind);
    setError(null);
    try {
      await startOAuthConnector(kind);
      onClose();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(msg);
    } finally {
      setPending(null);
    }
  };

  return (
    <div className="connector-modal-backdrop" onClick={onClose}>
      <div
        className="connector-modal"
        role="dialog"
        aria-modal="true"
        onClick={(e) => e.stopPropagation()}
      >
        <h3 className="connector-modal-title">Add connector</h3>
        <p className="connector-modal-sub">
          Pick a provider. Margin will open your browser to authenticate.
        </p>
        <div className="connector-modal-list">
          {providers.map((p) => (
            <button
              key={p.kind}
              type="button"
              className="connector-modal-row"
              onClick={() => onPick(p.kind)}
              disabled={pending !== null}
            >
              <span className="connector-modal-row-name">{p.display_name}</span>
              {pending === p.kind && (
                <span className="connector-modal-row-status">Waiting for browser…</span>
              )}
            </button>
          ))}
        </div>
        {error && <div className="connector-modal-error">{error}</div>}
        <div className="connector-modal-actions">
          <button
            type="button"
            className="connector-modal-cancel"
            onClick={onClose}
            disabled={pending !== null}
          >
            Close
          </button>
        </div>
      </div>
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

  const [hasFirecrawlKey, setHasFirecrawlKey] = useState<boolean>(false);
  const [firecrawlDraft, setFirecrawlDraft] = useState<string>("");
  const [firecrawlSaving, setFirecrawlSaving] = useState<boolean>(false);
  const [firecrawlError, setFirecrawlError] = useState<string | null>(null);

  const [hasVoyageKey, setHasVoyageKey] = useState<boolean>(false);
  const [voyageDraft, setVoyageDraft] = useState<string>("");
  const [voyageSaving, setVoyageSaving] = useState<boolean>(false);
  const [voyageError, setVoyageError] = useState<string | null>(null);
  const [embedStatus, setEmbedStatus] = useState<{
    state: string;
    done: number;
    remaining: number;
    errored: number;
    message: string | null;
  } | null>(null);

  useEffect(() => {
    void hasAnthropicApiKey().then(setHasKey);
    void hasFirecrawlApiKey().then(setHasFirecrawlKey);
    void hasVoyageApiKey().then(setHasVoyageKey);
    // Tail the embedding worker's status events for the progress pill (#104).
    let unlisten: (() => void) | null = null;
    void import("@tauri-apps/api/event").then(({ listen }) => {
      void listen("embed-status", (e) => {
        const p = e.payload as {
          state: string;
          done: number;
          remaining: number;
          errored: number;
          message: string | null;
        };
        setEmbedStatus(p);
      }).then((un) => {
        unlisten = un;
      });
    });
    return () => {
      if (unlisten) unlisten();
    };
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
    const ok = await ask("This will clear the Anthropic API key from your keychain. You can paste it back in any time.", {
      title: "Remove API key?",
      kind: "warning",
      okLabel: "Remove",
      cancelLabel: "Cancel",
    });
    if (!ok) return;
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

  const onSaveFirecrawl = async () => {
    const value = firecrawlDraft.trim();
    if (!value) return;
    setFirecrawlSaving(true);
    setFirecrawlError(null);
    try {
      await setFirecrawlApiKey(value);
      setFirecrawlDraft("");
      setHasFirecrawlKey(true);
    } catch (e) {
      setFirecrawlError(typeof e === "string" ? e : "Failed to save key");
    } finally {
      setFirecrawlSaving(false);
    }
  };

  const onRemoveFirecrawl = async () => {
    const ok = await ask(
      "This will clear the Firecrawl API key from your keychain. Workstream link summaries will stop populating until you paste it back.",
      {
        title: "Remove Firecrawl key?",
        kind: "warning",
        okLabel: "Remove",
        cancelLabel: "Cancel",
      },
    );
    if (!ok) return;
    setFirecrawlSaving(true);
    setFirecrawlError(null);
    try {
      await deleteFirecrawlApiKey();
      setHasFirecrawlKey(false);
    } catch (e) {
      setFirecrawlError(typeof e === "string" ? e : "Failed to remove key");
    } finally {
      setFirecrawlSaving(false);
    }
  };

  const onSaveVoyage = async () => {
    const value = voyageDraft.trim();
    if (!value) return;
    setVoyageSaving(true);
    setVoyageError(null);
    try {
      await setVoyageApiKey(value);
      setVoyageDraft("");
      setHasVoyageKey(true);
      // Kick off a backfill pass so the user sees immediate progress.
      void forceReindexEmbeddings();
    } catch (e) {
      setVoyageError(typeof e === "string" ? e : "Failed to save key");
    } finally {
      setVoyageSaving(false);
    }
  };

  const onRemoveVoyage = async () => {
    const ok = await ask(
      "This will clear the Voyage API key from your keychain. Semantic search will stop indexing new content; existing embeddings remain queryable.",
      {
        title: "Remove Voyage key?",
        kind: "warning",
        okLabel: "Remove",
        cancelLabel: "Cancel",
      },
    );
    if (!ok) return;
    setVoyageSaving(true);
    setVoyageError(null);
    try {
      await deleteVoyageApiKey();
      setHasVoyageKey(false);
    } catch (e) {
      setVoyageError(typeof e === "string" ? e : "Failed to remove key");
    } finally {
      setVoyageSaving(false);
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

      <div className="settings-row">
        <div className="settings-row-label">Firecrawl API key</div>
        <div className="settings-row-control settings-row-control--col">
          <div className="settings-actions">
            <input
              type="password"
              className="settings-input"
              placeholder={
                hasFirecrawlKey ? "•••••••• (saved in Keychain)" : "fc-…"
              }
              value={firecrawlDraft}
              onChange={(e) => setFirecrawlDraft(e.target.value)}
              autoComplete="off"
              spellCheck={false}
            />
            <button
              className="ghost"
              onClick={() => void onSaveFirecrawl()}
              disabled={firecrawlSaving || !firecrawlDraft.trim()}
            >
              Save
            </button>
            {hasFirecrawlKey && (
              <button
                className="ghost"
                onClick={() => void onRemoveFirecrawl()}
                disabled={firecrawlSaving}
              >
                Remove
              </button>
            )}
          </div>
          <div className="settings-row-control">
            <span
              className={"settings-status " + (hasFirecrawlKey ? "ok" : "muted")}
            >
              {hasFirecrawlKey ? "Configured" : "Not configured"}
            </span>
            <span className="settings-hint">
              Stored in macOS Keychain. Optional — without it, workstream
              link chips won't populate AI-generated summaries.
            </span>
          </div>
          {firecrawlError && (
            <div className="settings-error">{firecrawlError}</div>
          )}
        </div>
      </div>

      <div className="settings-row">
        <div className="settings-row-label">Voyage API key</div>
        <div className="settings-row-control settings-row-control--col">
          <div className="settings-actions">
            <input
              type="password"
              className="settings-input"
              placeholder={
                hasVoyageKey ? "•••••••• (saved in Keychain)" : "pa-…"
              }
              value={voyageDraft}
              onChange={(e) => setVoyageDraft(e.target.value)}
              autoComplete="off"
              spellCheck={false}
            />
            <button
              className="ghost"
              onClick={() => void onSaveVoyage()}
              disabled={voyageSaving || !voyageDraft.trim()}
            >
              Save
            </button>
            {hasVoyageKey && (
              <button
                className="ghost"
                onClick={() => void onRemoveVoyage()}
                disabled={voyageSaving}
              >
                Remove
              </button>
            )}
          </div>
          <div className="settings-row-control">
            <span
              className={"settings-status " + (hasVoyageKey ? "ok" : "muted")}
            >
              {hasVoyageKey ? "Configured" : "Not configured"}
            </span>
            <span className="settings-hint">
              Stored in macOS Keychain. Powers semantic retrieval across
              notes, emails, meetings, and workstreams. Get a key at
              voyageai.com.
            </span>
          </div>
          {embedStatus && (
            <>
              <div className="settings-row-control">
                <span className="settings-hint">
                  Embedding index:{" "}
                  {embedStatus.state === "syncing"
                    ? `syncing ${embedStatus.done}/${embedStatus.done + embedStatus.remaining}…`
                    : embedStatus.state === "idle"
                      ? `idle (${embedStatus.done} indexed this pass${embedStatus.errored ? `, ${embedStatus.errored} errored` : ""})`
                      : embedStatus.state === "needs_key"
                        ? "waiting for API key"
                        : embedStatus.state === "rate_limited"
                          ? "rate-limited — backing off"
                          : embedStatus.state}
                </span>
              </div>
              {embedStatus.message && (
                <div className="settings-error">
                  Embedding worker: {embedStatus.message}
                </div>
              )}
            </>
          )}
          {voyageError && <div className="settings-error">{voyageError}</div>}
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

// ----- Diagnostics (#135) -----------------------------------------------

function DiagnosticsSection() {
  const [metrics, setMetrics] = useState<ChatTurnMetric[]>([]);
  const [loading, setLoading] = useState(true);
  const [inspectFor, setInspectFor] = useState<ChatTurnMetric | null>(null);

  useEffect(() => {
    let cancelled = false;
    const refresh = async () => {
      try {
        const rows = await listChatTurnMetrics(100);
        if (!cancelled) {
          setMetrics(rows);
          setLoading(false);
        }
      } catch (e) {
        if (!cancelled) {
          console.error("[diagnostics] listChatTurnMetrics failed:", e);
          setLoading(false);
        }
      }
    };
    void refresh();
    const id = window.setInterval(() => void refresh(), 30_000);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
  }, []);

  return (
    <section className="settings-section">
      <h2>Diagnostics</h2>
      <p className="settings-section-intro">
        Per-turn telemetry for the AI chat. Useful for spotting stale answers,
        long tool dispatches, or zero-citation responses. Data is captured
        locally — nothing is uploaded.
      </p>
      <DiagnosticsAggregates metrics={metrics} />
      {loading ? (
        <p className="settings-placeholder">Loading…</p>
      ) : metrics.length === 0 ? (
        <p className="settings-placeholder">
          No turns recorded yet. Send a message in the chat — turns from
          this version forward will appear here.
        </p>
      ) : (
        <DiagnosticsTable
          metrics={metrics}
          onInspect={(m) => setInspectFor(m)}
        />
      )}
      {inspectFor && (
        <PromptInspector
          message={metricToSyntheticMessage(inspectFor)}
          emittedLabels={inspectFor.citations}
          onClose={() => setInspectFor(null)}
        />
      )}
    </section>
  );
}

function DiagnosticsAggregates({ metrics }: { metrics: ChatTurnMetric[] }) {
  const total = metrics.length;
  const avgLatency =
    total === 0
      ? 0
      : Math.round(
          metrics.reduce((acc, m) => acc + m.latency_ms, 0) / total,
        );
  const withTools = metrics.filter((m) => m.tool_call_count > 0).length;
  const noCitations = metrics.filter(
    (m) => m.citations.length === 0 && m.assistant_text_chars > 0,
  ).length;
  const pct = (n: number) =>
    total === 0 ? "—" : `${Math.round((100 * n) / total)}%`;
  return (
    <div className="diagnostics-aggregates">
      <AggCard label="Turns" value={total.toLocaleString()} />
      <AggCard label="Avg latency" value={`${avgLatency.toLocaleString()} ms`} />
      <AggCard label="Used tools" value={pct(withTools)} />
      <AggCard label="No citations" value={pct(noCitations)} />
    </div>
  );
}

function AggCard({ label, value }: { label: string; value: string }) {
  return (
    <div className="diagnostics-aggregate-card">
      <div className="diagnostics-aggregate-value">{value}</div>
      <div className="diagnostics-aggregate-label">{label}</div>
    </div>
  );
}

function DiagnosticsTable({
  metrics,
  onInspect,
}: {
  metrics: ChatTurnMetric[];
  onInspect: (m: ChatTurnMetric) => void;
}) {
  return (
    <div className="diagnostics-table-wrap">
      <table className="diagnostics-table">
        <thead>
          <tr>
            <th>Time</th>
            <th>Query</th>
            <th className="diagnostics-num">Srcs</th>
            <th className="diagnostics-num">Cite</th>
            <th className="diagnostics-num">Tools</th>
            <th className="diagnostics-num">Tokens</th>
            <th className="diagnostics-num">Latency</th>
          </tr>
        </thead>
        <tbody>
          {metrics.map((m) => (
            <DiagnosticsRow key={m.turn_id} metric={m} onInspect={onInspect} />
          ))}
        </tbody>
      </table>
    </div>
  );
}

function DiagnosticsRow({
  metric,
  onInspect,
}: {
  metric: ChatTurnMetric;
  onInspect: (m: ChatTurnMetric) => void;
}) {
  const time = new Date(metric.created_ms).toLocaleString();
  const compactTime = new Date(metric.created_ms).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
  const query = metric.query || "(no query stored)";
  const truncatedQuery = query.length > 80 ? query.slice(0, 77) + "…" : query;
  const tokens = formatTokensCell(metric);
  const tokensTooltip = formatTokensTooltip(metric);
  const noCitations =
    metric.citations.length === 0 && metric.assistant_text_chars > 0;
  const srcsTooltip = Object.entries(metric.sources_by_kind)
    .map(([k, v]) => `${k}: ${v}`)
    .join(" · ");
  return (
    <tr
      className={
        "diagnostics-row" +
        (metric.had_error_dispatch ? " diagnostics-row-warn" : "") +
        (noCitations ? " diagnostics-row-no-citations" : "")
      }
      onClick={() => onInspect(metric)}
      title={time}
    >
      <td className="diagnostics-time">{compactTime}</td>
      <td className="diagnostics-query">
        {truncatedQuery}
        {metric.had_error_dispatch && (
          <span className="diagnostics-warn-icon" title="A tool dispatch errored">
            {" ⚠"}
          </span>
        )}
      </td>
      <td className="diagnostics-num" title={srcsTooltip}>
        {metric.sources_total}
      </td>
      <td className="diagnostics-num">{metric.citations.length}</td>
      <td className="diagnostics-num">{metric.tool_call_count}</td>
      <td className="diagnostics-num" title={tokensTooltip}>
        {tokens}
      </td>
      <td className="diagnostics-num">
        {(metric.latency_ms / 1000).toFixed(metric.latency_ms < 1000 ? 2 : 1)}s
      </td>
    </tr>
  );
}

function formatTokens(n: number | null): string {
  if (n == null) return "—";
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return n.toString();
}

/// Compact cell for the Diagnostics tokens column. When the turn used
/// prompt caching (#142), surface the cache-read portion separately so
/// the savings are visible at a glance. Format: "17.0k🔁 + 0.3k → 0.4k"
/// reads as "served from cache + new + output". Cache-creation isn't
/// shown inline (it's a one-time event the user already paid for);
/// surfaced via the cell tooltip + the inspector instead.
function formatTokensCell(m: ChatTurnMetric): string {
  if (m.tokens_out == null) return "—";
  const read = m.cache_read_tokens ?? 0;
  const create = m.cache_creation_tokens ?? 0;
  const fresh = m.tokens_in ?? 0;
  if (read === 0 && create === 0) {
    return `${formatTokens(fresh)}/${formatTokens(m.tokens_out)}`;
  }
  return `${formatTokens(read)}🔁 + ${formatTokens(fresh)} → ${formatTokens(m.tokens_out)}`;
}

function formatTokensTooltip(m: ChatTurnMetric): string {
  if (m.tokens_out == null) return "No token data";
  const lines = [
    `Fresh input: ${formatTokens(m.tokens_in)}`,
    `Output: ${formatTokens(m.tokens_out)}`,
  ];
  if (m.cache_read_tokens != null && m.cache_read_tokens > 0) {
    lines.push(`Cache read: ${formatTokens(m.cache_read_tokens)} (0.1× rate)`);
  }
  if (m.cache_creation_tokens != null && m.cache_creation_tokens > 0) {
    lines.push(`Cache write: ${formatTokens(m.cache_creation_tokens)} (1.25× rate)`);
  }
  return lines.join("\n");
}

/// Construct a minimal ChatMessageView from a metric row so the
/// existing PromptInspector can render the citations check. The
/// inspector's `emittedLabels` override means we don't need the
/// assistant text itself — just the parsed labels (which the metric
/// row already carries).
function metricToSyntheticMessage(m: ChatTurnMetric): ChatMessageView {
  return {
    id: `metric-${m.turn_id}`,
    role: "assistant",
    parts: [{ kind: "text", value: "" }],
    status: "done",
    turnId: m.turn_id,
  };
}
