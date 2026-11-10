import { useSyncExternalStore } from "react";

const query = "(max-width: 650px)";

function subscribe(changed: () => void): () => void {
  const media = window.matchMedia?.(query);
  media?.addEventListener("change", changed);
  return () => media?.removeEventListener("change", changed);
}

export function useCompactLayout(): boolean {
  return useSyncExternalStore(subscribe, () => window.matchMedia?.(query).matches ?? false);
}

export type MobileScreen = "list" | "read" | "edit";
