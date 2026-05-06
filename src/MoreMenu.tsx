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
  onArchive,
  archived,
  onFavorite,
  favorited,
  onDuplicate,
}: {
  onClose: () => void;
  /** When omitted, the Delete item is hidden. */
  onDelete?: () => void;
  /** When omitted, the Archive item stays a no-op stub. */
  onArchive?: () => void;
  /** When true, the Archive item's label flips to "Move to notes". */
  archived?: boolean;
  /** When omitted, the Favorites item stays a no-op stub. */
  onFavorite?: () => void;
  /** When true, the Favorites item's label flips to "Remove from favorites". */
  favorited?: boolean;
  /** When omitted, the Duplicate item stays a no-op stub. */
  onDuplicate?: () => void;
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
      {cleaned.map((it) => {
        if (!("label" in it)) {
          return <div key="sep" className="nh-more-sep" />;
        }
        const isArchive = it.id === "arc";
        const isFavorite = it.id === "fav";
        const isDuplicate = it.id === "dup";
        const archiveWired = isArchive && onArchive !== undefined;
        const favoriteWired = isFavorite && onFavorite !== undefined;
        const duplicateWired = isDuplicate && onDuplicate !== undefined;
        const label = isArchive && archived
          ? "Move to notes"
          : isFavorite && favorited
            ? "Remove from favorites"
            : it.label;
        const wired =
          (it.id === "del" && onDelete !== undefined) ||
          archiveWired ||
          favoriteWired ||
          duplicateWired;
        return (
          <button
            key={it.id}
            type="button"
            className={"nh-more-item" + (it.danger ? " danger" : "")}
            title={wired ? label : `${label} — coming soon (issue #${it.issue})`}
            onClick={() => {
              if (it.id === "del" && onDelete) {
                onDelete();
                onClose();
                return;
              }
              if (archiveWired) {
                onArchive!();
                onClose();
                return;
              }
              if (favoriteWired) {
                onFavorite!();
                onClose();
                return;
              }
              if (duplicateWired) {
                onDuplicate!();
                onClose();
                return;
              }
              logStub(label, it.issue);
              onClose();
            }}
          >
            {it.icon}
            <span>{label}</span>
          </button>
        );
      })}
    </div>
  );
}
