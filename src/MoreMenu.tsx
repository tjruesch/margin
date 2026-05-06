import type React from "react";
import { IconArchive, IconCopy, IconSparkle, IconStar, IconTrash } from "./icons";

type Item =
  | { id: "sep" }
  | {
      id: string;
      label: string;
      icon: React.ReactNode;
      issue: number;
      danger?: boolean;
    };

const ITEMS: Item[] = [
  { id: "fav", icon: <IconStar size={14} />, label: "Add to favorites", issue: 16 },
  { id: "dup", icon: <IconCopy size={14} />, label: "Duplicate", issue: 18 },
  { id: "ai", icon: <IconSparkle size={14} />, label: "Summarize with AI", issue: 19 },
  { id: "sep" },
  { id: "arc", icon: <IconArchive size={14} />, label: "Archive", issue: 17 },
  { id: "del", icon: <IconTrash size={14} />, label: "Delete", issue: 20, danger: true },
];

function logStub(label: string, issue: number) {
  console.log(`[stub] ${label} clicked — see issue #${issue}`);
}

export function MoreMenu({
  onClose,
  onDelete,
}: {
  onClose: () => void;
  /** When omitted, the Delete item is hidden. */
  onDelete?: () => void;
}) {
  // Hide Delete when the parent hasn't authorized it, then drop any
  // separator that would dangle as a result.
  const visible = ITEMS.filter((it) => it.id !== "del" || onDelete);
  const cleaned = visible.filter((it, i, arr) => {
    if (it.id !== "sep") return true;
    const next = arr[i + 1];
    return next !== undefined && next.id !== "sep";
  });
  return (
    <div
      className="nh-popover nh-more-popover"
      onMouseDown={(e) => e.stopPropagation()}
      onClick={(e) => e.stopPropagation()}
    >
      {cleaned.map((it) =>
        "label" in it ? (
          <button
            key={it.id}
            type="button"
            className={"nh-more-item" + (it.danger ? " danger" : "")}
            title={
              it.id === "del" && onDelete
                ? it.label
                : `${it.label} — coming soon (issue #${it.issue})`
            }
            onClick={() => {
              if (it.id === "del" && onDelete) {
                onDelete();
                onClose();
                return;
              }
              logStub(it.label, it.issue);
              onClose();
            }}
          >
            {it.icon}
            <span>{it.label}</span>
          </button>
        ) : (
          <div key="sep" className="nh-more-sep" />
        ),
      )}
    </div>
  );
}
