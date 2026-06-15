import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useEffect, useMemo, useRef, useState } from "react";

import {
  createTodo,
  deleteTodo,
  listTodos,
  setTodoDone,
  type Todo,
  updateTodo,
} from "./file";
import { IconCheck, IconPlus, IconTrash } from "./icons";

// ---- date helpers --------------------------------------------------------

function pad(n: number): string {
  return String(n).padStart(2, "0");
}

/// ms → the value a <input type="datetime-local"> expects (local time).
function msToLocalInput(ms: number): string {
  const d = new Date(ms);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(
    d.getHours(),
  )}:${pad(d.getMinutes())}`;
}

function localInputToMs(v: string): number | null {
  if (!v) return null;
  const ms = new Date(v).getTime();
  return Number.isFinite(ms) ? ms : null;
}

function sameDay(a: number, b: number): boolean {
  const da = new Date(a);
  const db = new Date(b);
  return (
    da.getFullYear() === db.getFullYear() &&
    da.getMonth() === db.getMonth() &&
    da.getDate() === db.getDate()
  );
}

/// Friendly due label: "Today 3:00 PM", "Tomorrow 9:00 AM", "Mon, Jun 16".
function dueLabel(ms: number): string {
  const now = Date.now();
  const d = new Date(ms);
  const time = d.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  const tomorrow = new Date();
  tomorrow.setDate(tomorrow.getDate() + 1);
  if (sameDay(ms, now)) return `Today ${time}`;
  if (sameDay(ms, tomorrow.getTime())) return `Tomorrow ${time}`;
  const day = d.toLocaleDateString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
    year: d.getFullYear() === new Date().getFullYear() ? undefined : "numeric",
  });
  return `${day}, ${time}`;
}

type GroupKey = "overdue" | "today" | "upcoming" | "someday";
const GROUP_LABEL: Record<GroupKey, string> = {
  overdue: "Overdue",
  today: "Today",
  upcoming: "Upcoming",
  someday: "No date",
};

function groupActive(todos: Todo[], now: number): { key: GroupKey; items: Todo[] }[] {
  const buckets: Record<GroupKey, Todo[]> = {
    overdue: [],
    today: [],
    upcoming: [],
    someday: [],
  };
  for (const t of todos) {
    if (t.due_ms == null) buckets.someday.push(t);
    else if (t.due_ms < now) buckets.overdue.push(t);
    else if (sameDay(t.due_ms, now)) buckets.today.push(t);
    else buckets.upcoming.push(t);
  }
  return (["overdue", "today", "upcoming", "someday"] as GroupKey[])
    .filter((k) => buckets[k].length > 0)
    .map((k) => ({ key: k, items: buckets[k] }));
}

// ---- composer ------------------------------------------------------------

function TodoComposer({ onCreated }: { onCreated: () => void }) {
  const [text, setText] = useState("");
  const [due, setDue] = useState("");
  const [busy, setBusy] = useState(false);

  const add = async () => {
    const t = text.trim();
    if (!t || busy) return;
    setBusy(true);
    try {
      await createTodo(t, localInputToMs(due), "page");
      setText("");
      setDue("");
      onCreated();
    } catch (e) {
      console.error("[todos] create failed:", e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="todo-composer">
      <input
        className="todo-composer-input"
        placeholder="Add a todo…"
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            void add();
          }
        }}
        autoFocus
      />
      <input
        type="datetime-local"
        className="todo-composer-due"
        value={due}
        onChange={(e) => setDue(e.target.value)}
        title="Due date (optional)"
      />
      <button
        type="button"
        className="todo-composer-add"
        onClick={() => void add()}
        disabled={!text.trim() || busy}
      >
        <IconPlus size={13} sw={2} />
        Add
      </button>
    </div>
  );
}

// ---- row -----------------------------------------------------------------

function TodoRow({ todo, onChanged }: { todo: Todo; onChanged: () => void }) {
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(todo.text);
  const [editingDue, setEditingDue] = useState(false);
  const dueRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => setText(todo.text), [todo.text]);

  const overdue = !todo.done && todo.due_ms != null && todo.due_ms < Date.now();

  const saveText = async () => {
    const t = text.trim();
    setEditing(false);
    if (!t || t === todo.text) {
      setText(todo.text);
      return;
    }
    try {
      await updateTodo(todo.id, t, todo.due_ms);
      onChanged();
    } catch (e) {
      console.error("[todos] update failed:", e);
    }
  };

  const saveDue = async (ms: number | null) => {
    setEditingDue(false);
    try {
      await updateTodo(todo.id, todo.text, ms);
      onChanged();
    } catch (e) {
      console.error("[todos] set due failed:", e);
    }
  };

  return (
    <div className={"todo-row" + (todo.done ? " done" : "")}>
      <button
        type="button"
        className={"todo-check" + (todo.done ? " checked" : "")}
        onClick={() => void setTodoDone(todo.id, !todo.done).then(onChanged)}
        aria-label={todo.done ? "Mark not done" : "Mark done"}
      >
        {todo.done && <IconCheck size={11} sw={2.4} />}
      </button>

      <div className="todo-main">
        {editing ? (
          <input
            className="todo-edit-input"
            value={text}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void saveText();
              if (e.key === "Escape") {
                setText(todo.text);
                setEditing(false);
              }
            }}
            onBlur={() => void saveText()}
            autoFocus
          />
        ) : (
          <button
            type="button"
            className="todo-text"
            onClick={() => !todo.done && setEditing(true)}
            title={todo.done ? "" : "Click to edit"}
          >
            {todo.text}
          </button>
        )}

        {editingDue ? (
          <input
            ref={dueRef}
            type="datetime-local"
            className="todo-due-input"
            defaultValue={todo.due_ms != null ? msToLocalInput(todo.due_ms) : ""}
            onChange={(e) => void saveDue(localInputToMs(e.target.value))}
            onBlur={() => setEditingDue(false)}
            autoFocus
          />
        ) : todo.due_ms != null ? (
          <span className="todo-due-wrap">
            <button
              type="button"
              className={"todo-due" + (overdue ? " overdue" : "")}
              onClick={() => setEditingDue(true)}
            >
              {dueLabel(todo.due_ms)}
            </button>
            <button
              type="button"
              className="todo-due-clear"
              title="Remove due date"
              onClick={() => void saveDue(null)}
            >
              ×
            </button>
          </span>
        ) : (
          <button
            type="button"
            className="todo-due-add"
            onClick={() => setEditingDue(true)}
          >
            + Due date
          </button>
        )}
      </div>

      <button
        type="button"
        className="todo-delete"
        title="Delete"
        onClick={() => void deleteTodo(todo.id).then(onChanged)}
      >
        <IconTrash size={13} sw={1.7} />
      </button>
    </div>
  );
}

// ---- page ----------------------------------------------------------------

export function TodosView() {
  const [active, setActive] = useState<Todo[]>([]);
  const [completed, setCompleted] = useState<Todo[]>([]);
  const [tab, setTab] = useState<"active" | "completed">("active");
  const [loading, setLoading] = useState(true);
  const [now, setNow] = useState(() => Date.now());

  const refresh = async () => {
    try {
      const [a, c] = await Promise.all([listTodos("active"), listTodos("completed")]);
      setActive(a);
      setCompleted(c);
      setNow(Date.now());
    } catch (e) {
      console.error("[todos] refresh failed:", e);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void refresh();
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    void (async () => {
      const fn = await listen("todos-changed", () => void refresh());
      if (cancelled) fn();
      else unlisten = fn;
    })();
    // Re-evaluate overdue grouping each minute even without changes.
    const tick = setInterval(() => setNow(Date.now()), 60_000);
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
      clearInterval(tick);
    };
  }, []);

  const groups = useMemo(() => groupActive(active, now), [active, now]);

  return (
    <div className="todo-view">
      <TodoComposer onCreated={() => void refresh()} />

      <div className="todo-tabs">
        <button
          type="button"
          className={"todo-tab" + (tab === "active" ? " active" : "")}
          onClick={() => setTab("active")}
        >
          Active <span className="todo-tab-count">{active.length}</span>
        </button>
        <button
          type="button"
          className={"todo-tab" + (tab === "completed" ? " active" : "")}
          onClick={() => setTab("completed")}
        >
          Completed <span className="todo-tab-count">{completed.length}</span>
        </button>
      </div>

      {loading ? (
        <p className="settings-placeholder">Loading…</p>
      ) : tab === "active" ? (
        active.length === 0 ? (
          <div className="settings-empty">
            <div className="settings-empty-title">No todos yet</div>
            <div className="settings-empty-body">
              Add one above, or capture it anywhere with the command palette
              (⌘K) — by typing, or hold Space to dictate.
            </div>
          </div>
        ) : (
          <div className="todo-groups">
            {groups.map((g) => (
              <div key={g.key} className="todo-group">
                <div className={"todo-group-head todo-group-" + g.key}>
                  {GROUP_LABEL[g.key]}
                  <span className="todo-group-count">{g.items.length}</span>
                </div>
                <div className="todo-list">
                  {g.items.map((t) => (
                    <TodoRow key={t.id} todo={t} onChanged={() => void refresh()} />
                  ))}
                </div>
              </div>
            ))}
          </div>
        )
      ) : completed.length === 0 ? (
        <div className="settings-empty">
          <div className="settings-empty-title">Nothing completed yet</div>
          <div className="settings-empty-body">Finished todos land here.</div>
        </div>
      ) : (
        <div className="todo-list">
          {completed.map((t) => (
            <TodoRow key={t.id} todo={t} onChanged={() => void refresh()} />
          ))}
        </div>
      )}
    </div>
  );
}
