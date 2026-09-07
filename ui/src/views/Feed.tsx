/**
 * View 1: launches, their decision, and why (spec §8).
 *
 * Virtualised, because a busy hour is thousands of rows and the phase-5 criterion names
 * 20,000. The row height comes from `--h-row` in tokens.css, read once via
 * `getComputedStyle` — PLAN.md F8 — so the later design pass changes one token and both
 * the CSS and the virtualiser follow. There is no pixel value in this file.
 *
 * Until the engine lands in phase 6 this is the indexed window rather than a live tail.
 * The distinction is stated in the view rather than left for the user to discover.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { api, hasBackend, type FeedPage, type FeedRow } from "../ipc";
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

export function Feed() {
  const { passingOnly, togglePassingOnly, setError } = useApp();
  const [order, setOrder] = useState<Order>("time");
  const [search, setSearch] = useState("");
  const [page, setPage] = useState<FeedPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);

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

      {page?.truncated && (
        <div className="banner banner--info">
          Showing the newest {count(page.scanned)} launches. The store holds more; the
          Strategy Lab reads the whole window.
        </div>
      )}
      <div className="banner banner--info">
        These are indexed launches evaluated by the saved strategy, not a live tail. The
        engine arrives in phase 6.
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
        {row.passed ? "PASS" : "refused"}
      </span>
      <span className="col col--why">
        {/* Spec §3.4: the rule and the values, not just "rejected". */}
        {row.refusals.map((r) => r.rule).join(", ")}
      </span>
    </button>
  );
}
