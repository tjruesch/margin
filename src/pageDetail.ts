import { useLayoutEffect } from "react";

/// Signal to the page-header host that a detail view is showing.
/// Detail-screen components (Team-detail, Workstream-detail) call this
/// hook so `Home.tsx` can suppress its page-level H1 and list-scoped
/// actions ("+ Add team member" etc.) while a single item is on screen.
///
/// `useLayoutEffect` (rather than `useEffect`) so the page-header
/// suppression lands in the same paint as the detail mount — no
/// one-frame flicker of the parent header.
///
/// Implementation: dispatches a window-level CustomEvent on mount and
/// the paired close event on unmount. Home.tsx maintains a counter so
/// nested/overlapping detail mounts stay correct.
export function usePageDetailLifecycle(): void {
  useLayoutEffect(() => {
    window.dispatchEvent(new CustomEvent("margin:page-detail-open"));
    return () => {
      window.dispatchEvent(new CustomEvent("margin:page-detail-closed"));
    };
  }, []);
}
