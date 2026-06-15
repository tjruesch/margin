import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useEffect, useMemo, useState } from "react";

import {
  type ConnectorStatusEvent,
  type ContributionInsight,
  type GithubContribution,
  getContributionInsight,
  hasGithubConnector,
  listConnectors,
  listGithubContributions,
  syncConnectorNow,
} from "./file";
import { IconBrand } from "./icons";
import { render as renderMarkdown } from "./markdown";

type FilterId = "all" | "delivered" | "wip";

/// A merged PR is a "delivered feature"; an open / closed PR is work in
/// progress. (Commits were dropped — the changelog is PR-only.)
function isDelivered(c: GithubContribution): boolean {
  return c.state === "merged";
}

/// The timestamp the changelog sorts and groups by: merge time for
/// merged PRs, otherwise creation time.
function effectiveMs(c: GithubContribution): number {
  return c.merged_at_ms ?? c.created_at_ms;
}

function badge(c: GithubContribution): { label: string; cls: string } {
  if (c.state === "merged") return { label: "Delivered", cls: "delivered" };
  if (c.state === "open") return { label: "Open PR", cls: "open" };
  return { label: "Closed PR", cls: "closed" };
}

function dayKey(ms: number): string {
  const d = new Date(ms);
  return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
}

function dayLabel(ms: number): string {
  const d = new Date(ms);
  const today = new Date();
  const yesterday = new Date();
  yesterday.setDate(today.getDate() - 1);
  if (dayKey(ms) === dayKey(today.getTime())) return "Today";
  if (dayKey(ms) === dayKey(yesterday.getTime())) return "Yesterday";
  return d.toLocaleDateString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
    year: d.getFullYear() === today.getFullYear() ? undefined : "numeric",
  });
}

function timeLabel(ms: number): string {
  return new Date(ms).toLocaleTimeString(undefined, {
    hour: "numeric",
    minute: "2-digit",
  });
}

function fullDateLabel(ms: number): string {
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    year: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

type DayGroup = { key: string; label: string; items: GithubContribution[] };

function groupByDay(items: GithubContribution[]): DayGroup[] {
  const groups: DayGroup[] = [];
  let current: DayGroup | null = null;
  for (const c of items) {
    const ms = effectiveMs(c);
    const key = dayKey(ms);
    if (!current || current.key !== key) {
      current = { key, label: dayLabel(ms), items: [] };
      groups.push(current);
    }
    current.items.push(c);
  }
  return groups;
}

/// The GitHub changelog feed (#165). PR-only. Reads
/// `github_contributions`, refetches on every `connector-status` event,
/// and drills into a per-PR detail view with an AI summary + a high-bar
/// "worth writing about" highlight.
export function ChangelogView({ onOpenSettings }: { onOpenSettings: () => void }) {
  const [items, setItems] = useState<GithubContribution[]>([]);
  const [hasConnector, setHasConnector] = useState<boolean | null>(null);
  const [loading, setLoading] = useState(true);
  const [filter, setFilter] = useState<FilterId>("all");
  const [syncing, setSyncing] = useState(false);
  const [selectedId, setSelectedId] = useState<string | null>(null);

  const refresh = async () => {
    try {
      const [connected, list] = await Promise.all([
        hasGithubConnector(),
        listGithubContributions(),
      ]);
      setHasConnector(connected);
      setItems(list);
    } catch (e) {
      console.error("[changelog] refresh failed:", e);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void refresh();
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    void (async () => {
      const fn = await listen<ConnectorStatusEvent>("connector-status", () => {
        void refresh();
      });
      if (cancelled) fn();
      else unlisten = fn;
    })();
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, []);

  const onSyncNow = async () => {
    setSyncing(true);
    try {
      const conns = await listConnectors();
      const gh = conns.find((c) => c.kind === "github");
      if (gh) await syncConnectorNow(gh.id);
    } catch (e) {
      console.error("[changelog] sync now failed:", e);
    } finally {
      setTimeout(() => setSyncing(false), 1500);
    }
  };

  const deliveredCount = useMemo(() => items.filter(isDelivered).length, [items]);
  const wipCount = items.length - deliveredCount;

  const filtered = useMemo(() => {
    if (filter === "delivered") return items.filter(isDelivered);
    if (filter === "wip") return items.filter((c) => !isDelivered(c));
    return items;
  }, [items, filter]);

  const groups = useMemo(() => groupByDay(filtered), [filtered]);

  // Detail view takes over the panel when a PR is selected.
  const selected = useMemo(
    () => items.find((c) => c.id === selectedId) ?? null,
    [items, selectedId],
  );
  if (selected) {
    return (
      <ChangelogDetail
        contribution={selected}
        onBack={() => setSelectedId(null)}
      />
    );
  }

  return (
    <div className="changelog-view">
      <div className="changelog-toolbar">
        <div className="changelog-filters">
          <button
            type="button"
            className={"changelog-filter" + (filter === "all" ? " active" : "")}
            onClick={() => setFilter("all")}
          >
            All <span className="changelog-filter-count">{items.length}</span>
          </button>
          <button
            type="button"
            className={"changelog-filter" + (filter === "delivered" ? " active" : "")}
            onClick={() => setFilter("delivered")}
          >
            Delivered <span className="changelog-filter-count">{deliveredCount}</span>
          </button>
          <button
            type="button"
            className={"changelog-filter" + (filter === "wip" ? " active" : "")}
            onClick={() => setFilter("wip")}
          >
            In progress <span className="changelog-filter-count">{wipCount}</span>
          </button>
        </div>
        {hasConnector && (
          <button
            type="button"
            className="changelog-sync"
            onClick={onSyncNow}
            disabled={syncing}
            title="Pull the latest PRs now (otherwise polled every 15 min)"
          >
            {syncing ? "Syncing…" : "Sync now"}
          </button>
        )}
      </div>

      {loading ? (
        <p className="settings-placeholder">Loading…</p>
      ) : hasConnector === false ? (
        <div className="settings-empty">
          <div className="settings-empty-title">GitHub not connected</div>
          <div className="settings-empty-body">
            Connect a GitHub account in Settings → Connectors to build your
            changelog. Margin polls every 15 minutes for your pull requests —
            merged PRs are delivered features, open ones are work in progress —
            and backfills the last 30 days on first connect.
          </div>
          <button
            type="button"
            className="changelog-connect-cta"
            onClick={onOpenSettings}
          >
            Open Settings
          </button>
        </div>
      ) : items.length === 0 ? (
        <div className="settings-empty">
          <div className="settings-empty-title">No pull requests yet</div>
          <div className="settings-empty-body">
            Connected — the first sync (backfilling the last 30 days) runs
            within 15 seconds. Hit “Sync now” to pull immediately.
          </div>
        </div>
      ) : filtered.length === 0 ? (
        <div className="settings-empty">
          <div className="settings-empty-title">Nothing here</div>
          <div className="settings-empty-body">
            No {filter === "delivered" ? "delivered features" : "in-progress PRs"}{" "}
            in your changelog yet.
          </div>
        </div>
      ) : (
        <div className="changelog-groups">
          {groups.map((g) => (
            <div key={g.key} className="changelog-group">
              <div className="changelog-day">{g.label}</div>
              <div className="changelog-list">
                {g.items.map((c) => {
                  const b = badge(c);
                  return (
                    <button
                      key={c.id}
                      type="button"
                      className="changelog-row"
                      onClick={() => setSelectedId(c.id)}
                      title="View summary & insight"
                    >
                      <span className="changelog-icon changelog-icon-pr">
                        <IconBrand kind="github" size={15} />
                      </span>
                      <div className="changelog-row-body">
                        <div className="changelog-row-title">{c.title}</div>
                        <div className="changelog-row-meta">
                          <span className="changelog-repo">{c.repo}</span>
                          <span className="changelog-sep">·</span>
                          <span>{timeLabel(effectiveMs(c))}</span>
                          {c.ai_generated_ms != null && (
                            <span
                              className="changelog-insight-dot"
                              title="Insight ready"
                            >
                              ✦
                            </span>
                          )}
                        </div>
                      </div>
                      <span className={`changelog-badge changelog-badge-${b.cls}`}>
                        {b.label}
                      </span>
                    </button>
                  );
                })}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/// Per-PR detail: title, repo, GitHub link, the AI summary + optional
/// high-bar highlight, and the PR description rendered as markdown.
function ChangelogDetail({
  contribution,
  onBack,
}: {
  contribution: GithubContribution;
  onBack: () => void;
}) {
  const c = contribution;
  const b = badge(c);
  const [insight, setInsight] = useState<ContributionInsight | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = async (regenerate: boolean) => {
    setLoading(true);
    setError(null);
    try {
      const r = await getContributionInsight(c.id, regenerate);
      setInsight(r);
    } catch (e) {
      setError(
        typeof e === "string"
          ? e
          : e instanceof Error
          ? e.message
          : "Failed to generate insight",
      );
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void load(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [c.id]);

  const theme = document.documentElement.dataset.theme || "light";
  const when =
    c.state === "merged" && c.merged_at_ms != null
      ? `Merged ${fullDateLabel(c.merged_at_ms)}`
      : `Opened ${fullDateLabel(c.created_at_ms)}`;
  const bodyHtml = useMemo(
    () => (c.body ? renderMarkdown(c.body) : ""),
    [c.body],
  );

  return (
    <div className="changelog-detail">
      <button type="button" className="changelog-detail-back" onClick={onBack}>
        ← Changelog
      </button>

      <div className="changelog-detail-head">
        <div className="changelog-detail-badges">
          <span className={`changelog-badge changelog-badge-${b.cls}`}>{b.label}</span>
        </div>
        <h1 className="changelog-detail-title">{c.title}</h1>
        <div className="changelog-detail-meta">
          <span className="changelog-repo">{c.repo}</span>
          <span className="changelog-sep">·</span>
          <span>{when}</span>
          <span className="changelog-sep">·</span>
          <button
            type="button"
            className="changelog-detail-link"
            onClick={() => void openUrl(c.url)}
          >
            Open on GitHub ↗
          </button>
        </div>
      </div>

      <section className="changelog-insight">
        <div className="changelog-insight-head">
          <h2 className="changelog-insight-h">Summary</h2>
          {insight && !loading && (
            <button
              type="button"
              className="changelog-regen"
              onClick={() => void load(true)}
              title="Generate a fresh insight"
            >
              Regenerate
            </button>
          )}
        </div>

        {loading ? (
          <p className="changelog-insight-loading">Analyzing this PR…</p>
        ) : error ? (
          <div className="changelog-insight-error">
            {error.includes("API key")
              ? "Add your Anthropic API key in Settings → AI to generate changelog insights."
              : error}
            <button
              type="button"
              className="changelog-regen"
              onClick={() => void load(false)}
            >
              Retry
            </button>
          </div>
        ) : insight ? (
          <>
            <p className="changelog-insight-summary">{insight.summary}</p>
            {insight.highlight ? (
              <div className="changelog-highlight">
                <div className="changelog-highlight-tag">✦ Worth writing about</div>
                <div className="changelog-highlight-angle">
                  {insight.highlight.angle}
                </div>
                <p className="changelog-highlight-content">
                  {insight.highlight.content}
                </p>
              </div>
            ) : (
              <p className="changelog-insight-none">
                No standout angle for a post here.
              </p>
            )}
          </>
        ) : null}
      </section>

      {c.body && c.body.trim() ? (
        <section className="changelog-body-section">
          <h2 className="changelog-insight-h">Description</h2>
          <article
            className="markdown-body changelog-body"
            data-theme={theme}
            dangerouslySetInnerHTML={{ __html: bodyHtml }}
          />
        </section>
      ) : null}
    </div>
  );
}
