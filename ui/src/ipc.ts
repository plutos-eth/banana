/**
 * The only way out of the window.
 *
 * The CSP allows `connect-src 'self' ipc:` and nothing else (PLAN.md C1), so this file is
 * the complete list of things the frontend can cause to happen. Every entry maps to one
 * `#[tauri::command]` in `crates/app/src/commands.rs`; there is no `fetch` anywhere in
 * this codebase and there is no way to add one that would work.
 *
 * Types here mirror the Rust DTOs. They are hand-written rather than generated because
 * the surface is small and a generator would be one more thing to keep running; the view
 * tests in `crates/app/tests/views.rs` are what actually pin the shapes.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// --- shared shapes ------------------------------------------------------------------

export type Presence = "present" | "absent" | "unknown";

export interface Refusal {
  rule: string;
  detail: string;
}

export interface Socials {
  twitter: Presence;
  website: Presence;
  telegram: Presence;
}

/** Mirrors `quarrel_core::features::PitFeatures`. Everything a filter may read. */
export interface PitFeatures {
  pair: "eth" | { other: string };
  name: string;
  symbol: string;
  description: string;
  socials: Socials;
  /** `null` means the launch transaction was unreadable — never that the value is zero. */
  exempt_wallets: number | null;
  dev_buy_bps: number | null;
  creator_tax_bps: number | null;
  fee_recipient: "deployer" | "third_party" | "unknown";
  deployer_launches: number;
  deployer_graduations: number;
  fingerprint_twins_30m: number;
  deployer_history_depth_blocks: number;
}

export interface StoreSummary {
  path: string;
  exists: boolean;
  launches: number;
  trades: number;
  outcomes: number;
  from_block: number | null;
  to_block: number | null;
  hours_x10: number | null;
  bytes: number;
}

/** Chosen once on the startup screen and fixed for the life of the process. */
export type Mode = "test" | "live";

export interface Status {
  /** `null` until the startup screen has been answered. */
  mode: Mode | null;
  mode_label: string;
  /** What this mode means, in words the backend supplies. */
  engine: string;
  /** True only when armed. Keyed on rather than the label, so a new mode cannot render as safe. */
  can_spend: boolean;
  indexing: boolean;
  data_dir: string;
  store: StoreSummary;
  chain_id: number;
  explorer: string;
  has_saved_strategy: boolean;
  /** Whether the sniper is running right now. */
  engine_running: boolean;
  /** The wallet a saved key derives. Never the key: it does not cross this boundary. */
  wallet: string | null;
}

// --- feed ---------------------------------------------------------------------------

export interface FeedRow {
  token: string;
  symbol: string;
  name: string;
  block: number;
  ts: number | null;
  pair: string;
  dev_buy_bps: number | null;
  creator_tax_bps: number | null;
  exempt_wallets: number | null;
  twins: number;
  deployer_launches: number;
  passed: boolean;
  rank_bps: number;
  refusals: Refusal[];
}

export interface FeedPage {
  rows: FeedRow[];
  scanned: number;
  matched: number;
  truncated: boolean;
}

export interface FeedQuery {
  passing_only?: boolean;
  search?: string;
  limit?: number | null;
}

export interface RuleOutcome {
  rule: string;
  passed: boolean;
  detail: string | null;
}

export interface Links {
  token: string;
  curve: string;
  deployer: string;
  tx: string;
  socials: [string, string][];
}

export interface OutcomeView {
  entry_rule: string;
  has_entry: boolean;
  max_multiple_bps: number | null;
  mult_after_5m_bps: number | null;
  mult_after_30m_bps: number | null;
  migrated: boolean;
  post_entry_trades: number;
}

export interface LaunchDetail {
  row: FeedRow;
  features: PitFeatures;
  rules: RuleOutcome[];
  links: Links;
  outcome: OutcomeView | null;
}

// --- strategy -----------------------------------------------------------------------

export type Condition =
  | { kind: "require_twitter" }
  | { kind: "require_website" }
  | { kind: "require_telegram" }
  | { kind: "require_any_social" }
  | { kind: "dev_buy_bps"; min: number | null; max: number | null }
  | { kind: "max_creator_tax_bps"; bps: number }
  | { kind: "max_exempt_wallets"; max: number }
  | { kind: "fee_recipient_is"; recipient: string }
  | { kind: "min_deployer_grad_rate_bps"; bps: number; allow_unproven: boolean }
  | { kind: "max_deployer_launches"; max: number }
  | { kind: "max_fingerprint_twins"; max: number }
  | { kind: "pair_in"; pairs: unknown[] }
  | { kind: "keyword"; pattern: string };

/** Adjacently tagged, matching serde's `{op, of}` wire form for `EntryFilter`. */
export type EntryFilter =
  | { op: "all"; of: EntryFilter[] }
  | { op: "any"; of: EntryFilter[] }
  | { op: "not"; of: EntryFilter }
  | { op: "cond"; of: Condition };

export type SuccessTarget =
  | { kind: "fixed_hold_multiple"; minutes: number; multiple_bps: number }
  | { kind: "reached_migration" }
  | { kind: "max_multiple"; multiple_bps: number };

export interface StrategyConfig {
  entry_filter: EntryFilter;
  entry_model: {
    max_tax_bps: number;
    size_wei: string;
    slippage_bps: number;
    max_wait_ms: number;
  };
  success_target: SuccessTarget;
  exits: Record<string, unknown>;
  live_guards: Record<string, unknown>;
}

// --- backtest -----------------------------------------------------------------------

export interface Stage {
  id: string;
  label: string;
  of: string | null;
  remaining: number;
  removed: number;
}

export interface Percentiles {
  p10: number;
  p25: number;
  p50: number;
  p75: number;
  p90: number;
}

export interface HoldStats {
  assumption: string;
  measured_over: number;
  multiple: Percentiles;
  above_entry: number;
  below_entry: number;
}

export interface PeakStats {
  label: string;
  measured_over: number;
  multiple: Percentiles;
  never_above_entry: number;
}

/**
 * Spec §5.5's sample gate, as it arrives on the wire.
 *
 * Below thirty passing tokens the `measured` half of this union does not exist, so there
 * is no percentage field for a component to render by accident. That is the guard: it is
 * enforced in the Rust crate, and this type is the shape it produces.
 */
export type Results =
  | { status: "insufficient_sample"; passed: number; required: number; message: string }
  | {
      status: "measured";
      target: string;
      target_is_peak_based: boolean;
      hits: number;
      measured_over: number;
      hit_rate_bps: number;
      hold_5m: HoldStats;
      hold_30m: HoldStats;
      peak: PeakStats;
      migrations: number;
    };

export interface BacktestResult {
  window: {
    from_block: number;
    to_block: number;
    hours_x10: number;
    maturity_cutoff_hours: number;
  };
  funnel: { stages: Stage[] };
  results: Results;
  regime_warning: string;
  query_ms: number;
  deployer_depth_applied: boolean;
}

export interface PassCount {
  passed: number;
  matured: number;
  universe: number;
  query_ms: number;
}

// --- index --------------------------------------------------------------------------

export interface PhaseView {
  phase: string;
  from_block: number;
  last_block: number;
  target_block: number;
  rows_written: number;
  complete: boolean;
}

export interface IndexStatus {
  store: StoreSummary;
  indexing: boolean;
  phases: PhaseView[];
  undecodable: number;
  bundled: number;
  migrated: number;
}

export interface ProgressPayload {
  phase: string;
  units_done: number;
  units_total: number;
  percent_x10: number;
  rows_written: number;
  eta_secs: number | null;
}

export interface DonePayload {
  ok: boolean;
  error: string | null;
  elapsed_secs: number;
  launches: number;
  trades: number;
}

/**
 * One position. Money arrives as decimal strings because a `U256` does not fit a
 * JavaScript number, and a balance rounded to 53 bits of mantissa on the way to a screen
 * is the sort of quiet wrongness the money path exists to prevent. Format them; never
 * add them here.
 */
export interface PositionRow {
  token: string;
  symbol: string;
  pair: string;
  opened_at: number;
  closed_at: number | null;
  cost_wei: string;
  proceeds_wei: string;
  tokens_held: string;
  remaining_bps: number;
  /** Last mark, in bps of what the remainder cost. 10000 is break-even. */
  mult_bps: number | null;
  /** Best mark seen. NOT profit: see §5.4. Labelled as "peak", never as a gain. */
  peak_bps: number;
  close_reason: string | null;
  /** No transaction was ever sent for this position: it was a rehearsal. */
  simulated: boolean;
}

export interface RefusalCount {
  rule: string;
  count: number;
}

export interface Positions {
  open: PositionRow[];
  closed: PositionRow[];
  note: string;
  refusals: RefusalCount[];
  refusals_total: number;
}

/**
 * What the engine reports as it runs. The discriminant is `kind`; every variant carries
 * the reason for what it did, because the refusal is the product (§3.4).
 */
export type EngineEvent =
  | { kind: "started"; mode: Mode; wallet: string | null; coverage: string; notes: string[] }
  | { kind: "health"; head: number; behind_blocks: number; latency_ms: number; watched: number }
  | { kind: "gap"; detail: string }
  | { kind: "trouble"; detail: string; consecutive: number }
  | { kind: "seen"; token: string; block: number; age_ms: number }
  | { kind: "refused"; token: string; symbol: string; rule: string; detail: string }
  | { kind: "waiting"; token: string; symbol: string; tax_bps: number }
  | {
      kind: "entered";
      token: string;
      symbol: string;
      quote_wei: string;
      tokens: string;
      tax_bps: number;
      tax_paid_bps: number | null;
      tx_hash: string | null;
      simulated: boolean;
      detail: string;
    }
  | {
      kind: "marked";
      token: string;
      symbol: string;
      mult_bps: number;
      peak_bps: number;
      remaining_bps: number;
    }
  | {
      kind: "exited";
      token: string;
      symbol: string;
      rule: string;
      detail: string;
      sell_bps: number;
      quote_wei: string;
      tx_hash: string | null;
      simulated: boolean;
      closed: boolean;
    }
  | { kind: "failed"; token: string; symbol: string; detail: string }
  | { kind: "stopped"; detail: string; tax_summary: string };

// --- the calls ----------------------------------------------------------------------

export const api = {
  status: () => invoke<Status>("get_status"),
  strategy: () => invoke<StrategyConfig>("get_strategy"),
  saveStrategy: (config: StrategyConfig) => invoke<void>("save_strategy", { config }),
  feed: (query: FeedQuery) => invoke<FeedPage>("get_feed", { query }),
  launch: (token: string) => invoke<LaunchDetail>("get_launch", { token }),
  backtest: (config: StrategyConfig) => invoke<BacktestResult>("run_backtest", { config }),
  passCount: (config: StrategyConfig) => invoke<PassCount>("get_pass_count", { config }),
  positions: () => invoke<Positions>("get_positions"),
  indexStatus: () => invoke<IndexStatus>("get_index_status"),
  startIndex: (from: number | null, to: number | null) =>
    invoke<void>("start_index", { from, to }),
  openExplorer: (url: string) => invoke<void>("open_explorer", { url }),
  chooseMode: (mode: Mode) => invoke<Mode>("choose_mode", { mode }),
  startEngine: () => invoke<number>("start_engine"),
  stopEngine: () => invoke<boolean>("stop_engine"),
  saveKey: (key: string) => invoke<string>("save_key", { key }),
  clearKey: () => invoke<void>("clear_key"),
};

export const events = {
  indexProgress: (f: (p: ProgressPayload) => void): Promise<UnlistenFn> =>
    listen<ProgressPayload>("index-progress", (e) => f(e.payload)),
  indexDone: (f: (p: DonePayload) => void): Promise<UnlistenFn> =>
    listen<DonePayload>("index-done", (e) => f(e.payload)),
  engine: (f: (e: EngineEvent) => void): Promise<UnlistenFn> =>
    listen<EngineEvent>("engine", (e) => f(e.payload)),
  engineStopped: (f: () => void): Promise<UnlistenFn> => listen<null>("engine-stopped", () => f()),
};

/**
 * Whether the backend is reachable.
 *
 * `npm run dev` serves the UI in a plain browser for layout work, where there is no
 * Tauri runtime and every `invoke` rejects. Views render their empty state in that case
 * rather than an error, which is what makes the frontend developable without a build.
 */
export const hasBackend = (): boolean =>
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
