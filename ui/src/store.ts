/**
 * Cross-view state.
 *
 * Only what more than one view needs: which view is showing, the strategy (the Rules
 * editor writes it, the Feed and the Lab read it), and the status the mode indicator is
 * drawn from. Everything else is local to its view.
 *
 * The strategy here is a working copy. It becomes the saved `strategy.json` only when the
 * user saves — spec §7.2 makes that file the thing the sniper arms from, so editing a
 * stepper must not silently change what a running engine would do.
 */

import { create } from "zustand";
import { api, hasBackend, type StrategyConfig, type Status } from "./ipc";

export type ViewId = "feed" | "positions" | "lab" | "index" | "rules" | "status";

export const VIEWS: { id: ViewId; label: string; key: string }[] = [
  { id: "feed", label: "Feed", key: "1" },
  { id: "positions", label: "Positions", key: "2" },
  { id: "lab", label: "Strategy Lab", key: "3" },
  { id: "index", label: "Index", key: "4" },
  { id: "rules", label: "Rules", key: "5" },
  { id: "status", label: "Status", key: "6" },
];

interface AppStore {
  view: ViewId;
  setView: (v: ViewId) => void;

  status: Status | null;
  refreshStatus: () => Promise<void>;

  /** Last saved strategy, as the backend holds it. */
  saved: StrategyConfig | null;
  /** What the Rules editor is currently showing. Equal to `saved` when clean. */
  draft: StrategyConfig | null;
  dirty: boolean;
  setDraft: (c: StrategyConfig) => void;
  loadStrategy: () => Promise<void>;
  saveDraft: () => Promise<void>;
  revertDraft: () => void;

  /** Set by the Feed's `f` shortcut; read by the Feed. */
  passingOnly: boolean;
  togglePassingOnly: () => void;

  /** The last error any view saw, shown in the status bar until dismissed. */
  error: string | null;
  setError: (e: unknown) => void;
}

export const useApp = create<AppStore>((set, get) => ({
  view: "feed",
  setView: (view) => set({ view }),

  status: null,
  refreshStatus: async () => {
    if (!hasBackend()) return;
    try {
      set({ status: await api.status() });
    } catch (e) {
      get().setError(e);
    }
  },

  saved: null,
  draft: null,
  dirty: false,
  setDraft: (draft) =>
    set((s) => ({
      draft,
      dirty: JSON.stringify(draft) !== JSON.stringify(s.saved),
    })),
  loadStrategy: async () => {
    if (!hasBackend()) return;
    try {
      const saved = await api.strategy();
      set({ saved, draft: saved, dirty: false });
    } catch (e) {
      get().setError(e);
    }
  },
  saveDraft: async () => {
    const draft = get().draft;
    if (!draft) return;
    try {
      await api.saveStrategy(draft);
      set({ saved: draft, dirty: false });
      await get().refreshStatus();
    } catch (e) {
      get().setError(e);
    }
  },
  revertDraft: () => set((s) => ({ draft: s.saved, dirty: false })),

  passingOnly: false,
  togglePassingOnly: () => set((s) => ({ passingOnly: !s.passingOnly })),

  error: null,
  setError: (e) =>
    set({
      // Backend errors arrive as strings (see `AppError`'s Serialize impl); anything
      // else is shown as-is rather than swallowed.
      error: e === null ? null : typeof e === "string" ? e : String(e),
    }),
}));
