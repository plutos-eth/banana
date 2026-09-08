/**
 * View 1: launches, their decision, and why (spec §8).
 *
 * Virtualised, because a busy hour is thousands of rows and the phase-5 criterion names
 * 20,000. The row height comes from `--h-row` in tokens.css, read once via
 * `getComputedStyle` — PLAN.md F8 — so the later design pass changes one token and both
 * the CSS and the virtualiser follow. There is no pixel value in this file.
 *
 * Two lists, and the difference between them is stated rather than left to be discovered:
 *
 * * **Live** — what the running engine is seeing *now*, newest first, arriving as it
 *   happens. Nothing here is in the store yet; it exists only for this session.
 * * **Indexed** — the window `history.db` covers, evaluated by the saved strategy. This is
 *   the one that is virtualised, searchable and rankable, because it is thousands of rows.
 *
 * Collapsing them into one list was the obvious thing and it is wrong: a row in the first
 * has no outcome and never will until it is indexed, and a row in the second is a fact
 * about the past. Showing them as one would make "decision" mean two different things in
 * the same column.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { api, hasBackend, type EngineEvent, type FeedPage, type FeedRow } from "../ipc";
import { bps, clock, count, shortHex } from "../format";
import { useApp } from "../store";
import { rowHeight } from "../rowHeight";
import { Drawer } from "../components/Drawer";
import { Empty } from "../components/Empty";

/**
 * How the list is ordered.
 *
 * `rank` is spec §7.1's answer: with the default rules well over 99% of launches are
 * refused and an unordered wall of refusals is unusable, so the nearest to passing come
 * first. That is right for reviewing a window.
 *
 * It is wrong for watching one. A live tail that inserts a new launch in the middle of the
 * list according to how nearly it passed is not a tail, and when the engine lands in phase
 * 6 that is exactly what would happen. So the order is a choice, and `time` is what a feed
 * means by default.
 */
type Order = "time" | "rank";

/**
 * How many live rows are drawn.
 *
 * A tail, not an archive: the store keeps 500 events and `live.db` keeps every one, so
 * this is only about how much of it is worth having on screen at once.
 */
const LIVE_CAP = 40;

/** One token as the running engine has seen it, folded from its events. */
interface LiveRow {
  token: string;
  symbol: string;
  block: number | null;
  ageMs: number | null;
  status: "seen" | "refused" | "waiting" | "entered" | "exited" | "failed";
  rule: string;
  detail: string;
  /** Position in the activity list, so the newest stays at the top. */
  seq: number;
}

const HAS_TOKEN = ["seen", "refused", "waiting", "entered", "exited", "failed"];

/**
 * Fold the engine's event stream into one row per token.
 *
 * The stream is newest-first, so the **first** event met for a token is its current state
 * and every later one only fills in what is still missing — the symbol comes from the
 * refusal, the block and the age from the sighting that preceded it.
 */
function foldLive(activity: EngineEvent[]): LiveRow[] {
  const byToken = new Map<string, LiveRow>();
  activity.forEach((e, i) => {
    if (!HAS_TOKEN.includes(e.kind)) return;
    const token = (e as { token: string }).token;
    let row = byToken.get(token);
    if (!row) {
      row = {
        token,
        symbol: "",
        block: null,
        ageMs: null,
        status: e.kind as LiveRow["status"],
        rule: "",
        detail: "",
        seq: i,
      };
      byToken.set(token, row);
    }
    switch (e.kind) {
      case "seen":
        if (row.block === null) {
          row.block = e.block;
          row.ageMs = e.age_ms;
        }
        break;
      case "refused":
        if (!row.symbol) row.symbol = e.symbol;
        if (!row.rule) {
          row.rule = e.rule;
          row.detail = e.detail;
        }
        break;
      case "waiting":
        if (!row.symbol) row.symbol = e.symbol;
        if (!row.detail) {
          row.detail = `passed every rule; waiting for the opening tax to fall from ${e.tax_bps} bps`;
        }
        break;
      case "entered":
        if (!row.symbol) row.symbol = e.symbol;
        if (!row.detail) {
          row.detail = `${e.simulated ? "would have bought" : "bought"} at ${e.tax_bps} bps tax`;
        }
        break;
      case "exited":
        if (!row.symbol) row.symbol = e.symbol;
        if (!row.rule) {
          row.rule = e.rule;
          row.detail = e.detail;
        }
        break;
      case "failed":
        if (!row.symbol) row.symbol = e.symbol;
        if (!row.detail) row.detail = e.detail;
        break;
    }
  });
  return [...byToken.values()].sort((a, b) => a.seq - b.seq);
}

/** Whether a live row got past the entry filter. */
const passedFilter = (r: LiveRow) =>
  r.status === "waiting" || r.status === "entered" || r.status === "exited";

export function Feed() {
  const { passingOnly, togglePassingOnly, setError, activity, status } = useApp();
  const [order, setOrder] = useState<Order>("time");
  const [search, setSearch] = useState("");
  const [page, setPage] = useState<FeedPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);

  // Derived from the engine's own event stream, which the shell keeps in the store so it
  // survives a view change. No second subscription and no second source of truth.
  const live = useMemo(() => foldLive(activity), [activity]);

  const load = useCallback(async () => {
    if (!hasBackend()) return;
    setLoading(true);
    try {
      setPage(await api.feed({ passing_only: passingOnly, search }));
    } catch (e) {
      setError(e);
      setPage(null);
    } finally {
      setLoading(false);
    }
  }, [passingOnly, search, setError]);

  useEffect(() => {
    void load();
  }, [load]);

  // `f` and `/` belong to this view; `esc` closes the drawer (spec §8).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const typing =
        e.target instanceof HTMLElement && ["INPUT", "TEXTAREA"].includes(e.target.tagName);
      if (e.key === "Escape") {
        if (typing) searchRef.current?.blur();
        else setSelected(null);
        return;
      }
      if (typing) return;
      if (e.key === "f") {
        e.preventDefault();
        togglePassingOnly();
      }
      if (e.key === "/") {
        e.preventDefault();
        searchRef.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [togglePassingOnly]);

  // The backend returns them nearest-first; re-sorting by block is cheap and keeps the
  // ordering a view concern rather than a second thing the API has to know about.
  const rows = useMemo(() => {
    const r = page?.rows ?? [];
    return order === "time" ? [...r].sort((a, b) => b.block - a.block) : r;
  }, [page, order]);
  const estimate = useMemo(() => rowHeight(), []);
  const virtual = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => estimate,
    overscan: 12,
  });

  return (
    <div className="view">
      <div className="toolbar">
        <span className="toolbar__title">Feed</span>
        <button
          type="button"
          className={`chip${passingOnly ? " is-on" : ""}`}
          onClick={togglePassingOnly}
        >
          {passingOnly ? "passing only" : "all launches"} <kbd className="kbd">f</kbd>
        </button>
        <input
          ref={searchRef}
          className="input"
          placeholder="search name, symbol or address    /"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          spellCheck={false}
        />
        <button
          type="button"
          className="chip"
          onClick={() => setOrder(order === "time" ? "rank" : "time")}
          title={
            order === "time"
              ? "Newest first, which is what a feed means."
              : "Nearest to passing first (§7.1), for reading a whole window."
          }
        >
          {order === "time" ? "newest first" : "nearest first"}
        </button>
        <span className="toolbar__spacer" />
        <span className="toolbar__note mono">
          {loading ? "reading…" : `${count(page?.matched ?? 0)} of ${count(page?.scanned ?? 0)}`}
        </span>
        <button type="button" className="btn" onClick={() => void load()}>
          refresh
        </button>
      </div>

      <LiveTail
        rows={live}
        running={status?.engine_running === true}
        passingOnly={passingOnly}
      />

      <div className="banner banner--info">
        The <b>indexed</b> window, evaluated by the saved strategy — the past, with outcomes.
        {page?.truncated &&
          ` Showing the newest ${count(page.scanned)}; the store holds more, and the Strategy Lab reads the whole window.`}
      </div>

      {!hasBackend() ? (
        <Empty title="No backend" note="Run the desktop app to see real launches." />
      ) : rows.length === 0 && !loading ? (
        <Empty
          title="Nothing to show"
          note={
            passingOnly
              ? "No indexed launch passes the saved strategy. Loosen a rule in Rules, or index a wider window."
              : "No launches in the store. Run an index from the Index view."
          }
        />
      ) : (
        <div className="table">
          <div className="table__head">
            <span className="col col--time">time</span>
            <span className="col col--sym">symbol</span>
            <span className="col col--name">name</span>
            <span className="col col--pair">pair</span>
            <span className="col col--num">dev buy</span>
            <span className="col col--num">tax</span>
            <span className="col col--num">bundle</span>
            <span className="col col--num">twins</span>
            <span className="col col--decision">decision</span>
            <span className="col col--why">why</span>
          </div>
          <div className="table__body" ref={scrollRef}>
            <div className="table__spacer" style={{ height: virtual.getTotalSize() }}>
              {virtual.getVirtualItems().map((v) => {
                const r = rows[v.index];
                if (!r) return null;
                return (
                  <Row
                    key={r.token}
                    row={r}
                    top={v.start}
                    height={v.size}
                    onOpen={() => setSelected(r.token)}
                  />
                );
              })}
            </div>
          </div>
        </div>
      )}

      {selected && <Drawer token={selected} onClose={() => setSelected(null)} />}
    </div>
  );
}

function Row({
  row,
  top,
  height,
  onOpen,
}: {
  row: FeedRow;
  top: number;
  height: number;
  onOpen: () => void;
}) {
  return (
    <button
      type="button"
      className={`row${row.passed ? " row--pass" : ""}`}
      style={{ transform: `translateY(${top}px)`, height }}
      onClick={onOpen}
    >
      <span className="col col--time mono">{clock(row.ts)}</span>
      <span className="col col--sym mono">{row.symbol || shortHex(row.token)}</span>
      <span className="col col--name">{row.name}</span>
      <span className="col col--pair mono">{row.pair}</span>
      <span className="col col--num mono">{bps(row.dev_buy_bps)}</span>
      <span className="col col--num mono">{bps(row.creator_tax_bps)}</span>
      <span className="col col--num mono">
        {row.exempt_wallets === null ? "?" : row.exempt_wallets}
      </span>
      <span className="col col--num mono">{row.twins}</span>
      <span className={`col col--decision ${row.passed ? "is-pass" : "is-refuse"}`}>
        {row.passed ? "pass" : "refused"}
      </span>
      <span className="col col--why">
        {/* Spec §3.4: the rule and the values, not just "rejected". A passing row gets a
            dash rather than nothing — an empty cell reads as a value that failed to load. */}
        {row.passed ? "—" : row.refusals.map((r) => r.rule).join(", ")}
      </span>
    </button>
  );
}

/**
 * What the engine is seeing right now.
 *
 * Deliberately not virtualised and deliberately capped: this is a tail, not an archive.
 * The archive is the table underneath, and the whole record is in `live.db`.
 */
function LiveTail({
  rows,
  running,
  passingOnly,
}: {
  rows: LiveRow[];
  running: boolean;
  passingOnly: boolean;
}) {
  const shown = (passingOnly ? rows.filter(passedFilter) : rows).slice(0, LIVE_CAP);

  if (!running && rows.length === 0) {
    return (
      <div className="banner banner--info">
        The engine is not running, so there is no live tail. Press <kbd className="kbd">p</kbd>{" "}
        to start it — in TEST it runs every step and stops at the signature.
      </div>
    );
  }

  return (
    <section className="live">
      <div className="live__head">
        <span className="live__title">
          <span className={`pulse ${running ? "pulse--live" : "pulse--dead"}`} /> live
        </span>
        <span className="toolbar__note">
          {running
            ? `${count(rows.length)} launches this session, newest first`
            : "the engine has stopped; these are what it saw"}
        </span>
      </div>
      {shown.length === 0 ? (
        <p className="note">
          {passingOnly
            ? "Nothing has passed the filter yet this session. Press f to see every launch."
            : "Watching. Launches arrive every few seconds."}
        </p>
      ) : (
        <div className="datawrap">
          <table className="data">
            <thead>
              <tr>
                <th className="data__num">age</th>
                <th>symbol</th>
                <th>token</th>
                <th className="data__num">block</th>
                <th>decision</th>
                <th>why</th>
              </tr>
            </thead>
            <tbody>
              {shown.map((r) => (
                <tr key={r.token} className={`live__row live__row--${r.status}`}>
                  <td className="data__num mono">{r.ageMs === null ? "" : `${r.ageMs} ms`}</td>
                  <td className="mono">{r.symbol || "…"}</td>
                  <td className="mono note">{shortHex(r.token)}</td>
                  <td className="data__num mono">{r.block === null ? "" : count(r.block)}</td>
                  <td className="mono">{decisionOf(r)}</td>
                  <td className="note">{r.detail}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

/**
 * The decision in one word, plus the rule that produced it.
 *
 * "reading" is a real state and not a blank: a launch whose enrichment is still in flight
 * has not been refused, and showing nothing there would read as one.
 */
function decisionOf(r: LiveRow): string {
  switch (r.status) {
    case "seen":
      return "reading";
    case "refused":
      return `refused · ${r.rule}`;
    case "waiting":
      return "passed";
    case "entered":
      return "bought";
    case "exited":
      return `sold · ${r.rule}`;
    case "failed":
      return "failed";
  }
}
