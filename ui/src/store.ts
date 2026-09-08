/**
 * Cross-view state.
 *
 * Only what more than one view needs: which view is showing, the strategy (the rule
 * editor in the Lab writes it, the Feed reads it), and the status the mode indicator is
 * drawn from. Everything else is local to its view.
 *
 * The strategy here is a working copy. It becomes the saved `strategy.json` only when the
 * user saves — spec §7.2 makes that file the thing the sniper arms from, so editing a
 * stepper must not silently change what a running engine would do.
 */

import { create } from "zustand";
import {
  api,
  hasBackend,
  type DonePayload,
  type EngineEvent,
  type Positions,
  type ProgressPayload,
  type StrategyConfig,
  type Status,
} from "./ipc";

/**
 * How many engine events are kept.
 *
 * A session runs for hours and sees a launch every few seconds, so this is a window, not
 * a log — the journal in `live.db` is the record, and it keeps everything.
 */
export const ACTIVITY_CAP = 500;

export type ViewId = "feed" | "positions" | "lab" | "index" | "status";

export const VIEWS: { id: ViewId; label: string; key: string }[] = [
  { id: "feed", label: "Feed", key: "1" },
  { id: "positions", label: "Positions", key: "2" },
  { id: "lab", label: "Strategy Lab", key: "3" },
  { id: "index", label: "Index", key: "4" },
  { id: "status", label: "Status", key: "5" },
];

interface AppStore {
  view: ViewId;
  setView: (v: ViewId) => void;

  status: Status | null;
  refreshStatus: () => Promise<void>;

  /** Last saved strategy, as the backend holds it. */
  saved: StrategyConfig | null;
  /** What the rule editor is currently showing. Equal to `saved` when clean. */
  draft: StrategyConfig | null;
  dirty: boolean;
  setDraft: (c: StrategyConfig) => void;
  loadStrategy: () => Promise<void>;
  saveDraft: () => Promise<void>;
  revertDraft: () => void;

  /**
   * The running index, or `null` when none is.
   *
   * Shared rather than local to the Index view for two reasons. An index takes tens of
   * minutes, so a user who switches views mid-run would otherwise see no sign it is
   * happening; and the events fire whether or not that view is mounted, so state kept
   * there is lost on every view change and only reappears at the next event.
   */
  indexProgress: ProgressPayload | null;
  /** The outcome of the last index this session, shown until another starts. */
  indexDone: DonePayload | null;
  setIndexProgress: (p: ProgressPayload | null) => void;
  setIndexDone: (d: DonePayload | null) => void;

  /**
   * What the engine has said, newest first, capped at {@link ACTIVITY_CAP}.
   *
   * Held here rather than in a view because the engine outlives any view: switching
   * screens mid-session must not lose the record of what happened while you were away.
   */
  activity: EngineEvent[];
  /** The last health pulse, so a quiet feed can be told from a broken one. */
  pulse: Extract<EngineEvent, { kind: "health" }> | null;
  pushEngineEvent: (e: EngineEvent) => void;
  clearActivity: () => void;

  /** Positions as `live.db` has them. Re-read whenever the engine moves one. */
  positions: Positions | null;
  refreshPositions: () => Promise<void>;
  /** Start the engine if it is stopped, stop it if it is running. */
  toggleEngine: () => Promise<void>;

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

  indexProgress: null,
  indexDone: null,
  setIndexProgress: (indexProgress) => set({ indexProgress }),
  setIndexDone: (indexDone) => set({ indexDone, indexProgress: null }),

  activity: [],
  pulse: null,
  pushEngineEvent: (e) =>
    set((s) => ({
      // The pulse is separate: it fires every poll and would otherwise crowd every
      // launch, refusal and fill out of the window within seconds.
      pulse: e.kind === "health" ? e : s.pulse,
      activity:
        e.kind === "health" ? s.activity : [e, ...s.activity].slice(0, ACTIVITY_CAP),
    })),
  clearActivity: () => set({ activity: [] }),

  positions: null,
  refreshPositions: async () => {
    if (!hasBackend()) return;
    try {
      set({ positions: await api.positions() });
    } catch (e) {
      get().setError(e);
    }
  },
  toggleEngine: async () => {
    if (!hasBackend()) return;
    try {
      if (get().status?.engine_running) {
        await api.stopEngine();
      } else {
        // Clearing first means the activity list belongs to the run you are watching,
        // rather than mixing two sessions into one scroll.
        set({ activity: [], pulse: null });
        await api.startEngine();
      }
      await get().refreshStatus();
      await get().refreshPositions();
    } catch (e) {
      get().setError(e);
    }
  },

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
