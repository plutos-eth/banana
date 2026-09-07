/**
 * View 2: positions, and what the engine is doing.
 *
 * Three things, in the order a user asks for them:
 *
 * 1. **What am I holding**, with the mark being what selling it would actually fetch.
 * 2. **What just happened**, as a live activity list.
 * 3. **Why nothing fired**, as refusals grouped by rule. That third one is the reason
 *    this view is worth opening on a quiet afternoon (spec §3.4).
 *
 * Spec §5.4: the peak is shown as a peak and never as profit. `cost` and `returned` are
 * the money columns; `peak` sits apart from them and is labelled for what it is.
 */

import { useCallback, useEffect } from "react";
import { hasBackend, type EngineEvent, type PositionRow } from "../ipc";
import { multiple, shortHex } from "../format";
import { useApp } from "../store";
import { Empty } from "../components/Empty";

/** Wei to a readable ETH figure, without ever going through a float. */
function eth(wei: string, places = 4): string {
  const s = wei.padStart(19, "0");
  const whole = s.slice(0, -18).replace(/^0+(?=\d)/, "");
  const frac = s.slice(-18).slice(0, places);
  return `${whole}.${frac}`;
}

export function Positions() {
  const { positions, refreshPositions, activity, status } = useApp();

  const load = useCallback(() => {
    if (!hasBackend()) return;
    void refreshPositions();
  }, [refreshPositions]);

  useEffect(() => {
    load();
  }, [load]);

  // Re-read whenever the engine reports something this view shows. A refusal counts:
  // the "why nothing fired" table is the whole reason to have this open on a quiet
  // afternoon, and it is exactly the case where no position ever changes.
  useEffect(() => {
    const last = activity[0];
    const shown = ["entered", "exited", "marked", "refused", "started", "stopped"];
    if (last && shown.includes(last.kind)) {
      load();
    }
  }, [activity, load]);

  if (!hasBackend()) {
    return <Empty title="No backend" note="Run the desktop app." />;
  }

  const running = status?.engine_running === true;

  return (
    <div className="view view--scroll">
      <div className="toolbar">
        <span className="toolbar__title">Positions</span>
        <span className="toolbar__spacer" />
        <EngineButton />
      </div>

      {positions?.note && <p className="note">{positions.note}</p>}

      {positions && positions.open.length > 0 && (
        <section className="panel">
          <h2 className="panel__title">Open</h2>
          <PositionTable rows={positions.open} />
        </section>
      )}

      <section className="panel">
        <h2 className="panel__title">Activity</h2>
        {activity.length === 0 ? (
          <p className="note">
            {running
              ? "Watching. Nothing has come through yet — launches arrive every few seconds."
              : "The engine has not run this session."}
          </p>
        ) : (
          <ul className="activity">
            {activity.slice(0, 60).map((e, i) => (
              <li key={i} className={`activity__row activity__row--${e.kind}`}>
                <ActivityLine e={e} />
              </li>
            ))}
          </ul>
        )}
      </section>

      {positions && positions.refusals.length > 0 && (
        <section className="panel">
          <h2 className="panel__title">Why nothing fired</h2>
          <p className="note">
            {positions.refusals_total} launches were refused this session. The rule that
            stopped each one, commonest first — if the top line is not the rule you meant to
            be strict about, that is the one to loosen.
          </p>
          <table className="kv">
            <tbody>
              {positions.refusals.map((r) => (
                <tr key={r.rule}>
                  <th>{r.rule}</th>
                  <td className="mono">{r.count}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      )}

      {positions && positions.closed.length > 0 && (
        <section className="panel">
          <h2 className="panel__title">Closed</h2>
          <PositionTable rows={positions.closed} closed />
        </section>
      )}
    </div>
  );
}

function PositionTable({ rows, closed }: { rows: PositionRow[]; closed?: boolean }) {
  return (
    <div className="datawrap">
      <table className="data">
        <thead>
          <tr>
            <th>token</th>
            <th className="data__num">cost</th>
            <th className="data__num">returned</th>
            <th className="data__num">left</th>
            <th className="data__num">mark</th>
            <th className="data__num">peak</th>
            {closed && <th>closed because</th>}
          </tr>
        </thead>
        <tbody>
          {rows.map((p) => (
            <tr key={p.token}>
              <td>
                <span className="mono">{p.symbol || shortHex(p.token)}</span>
                {p.simulated && <span className="tag tag--test">TEST</span>}
              </td>
              <td className="data__num mono">{eth(p.cost_wei)}</td>
              <td className="data__num mono">{eth(p.proceeds_wei)}</td>
              <td className="data__num mono">{(p.remaining_bps / 100).toFixed(0)}%</td>
              <td className="data__num mono">{multiple(p.mult_bps)}</td>
              {/* Spec §5.4: this is the best it was worth, not money made. */}
              <td className="data__num mono note" title="the best this was ever worth, not profit">
                {multiple(p.peak_bps)}
              </td>
              {closed && <td className="note">{p.close_reason ?? ""}</td>}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function ActivityLine({ e }: { e: EngineEvent }) {
  switch (e.kind) {
    case "started":
      return (
        <>
          <b>started</b> {e.mode.toUpperCase()}
          {e.wallet ? ` as ${shortHex(e.wallet)}` : " with no wallet"} — {e.coverage}
          {e.notes.map((n, i) => (
            <div key={i} className="note">
              {n}
            </div>
          ))}
        </>
      );
    case "seen":
      return (
        <>
          <span className="mono">{shortHex(e.token)}</span> launched in block {e.block},{" "}
          {e.age_ms} ms old
        </>
      );
    case "refused":
      return (
        <>
          <b>refused</b> {e.symbol || shortHex(e.token)} — <span className="mono">{e.rule}</span>:{" "}
          {e.detail}
        </>
      );
    case "waiting":
      return (
        <>
          <b>passed</b> {e.symbol || shortHex(e.token)} — waiting for the opening tax to fall
          from {e.tax_bps} bps
        </>
      );
    case "entered":
      return (
        <>
          <b>{e.simulated ? "would have bought" : "bought"}</b> {e.symbol} for{" "}
          {eth(e.quote_wei)} ETH at {e.tax_bps} bps tax
          {e.tax_paid_bps !== null && ` (paid ${e.tax_paid_bps} bps)`} — {e.detail}
        </>
      );
    case "marked":
      return (
        <>
          {e.symbol} at {multiple(e.mult_bps)}, peak {multiple(e.peak_bps)}
        </>
      );
    case "exited":
      return (
        <>
          <b>{e.simulated ? "would have sold" : "sold"}</b> {(e.sell_bps / 100).toFixed(0)}% of{" "}
          {e.symbol} for {eth(e.quote_wei)} ETH — <span className="mono">{e.rule}</span>: {e.detail}
        </>
      );
    case "failed":
      return (
        <>
          <b>failed</b> {e.symbol || shortHex(e.token)} — {e.detail}
        </>
      );
    case "gap":
      return <b>{e.detail}</b>;
    case "trouble":
      return (
        <>
          <b>feed trouble</b> ({e.consecutive} in a row) — {e.detail}
        </>
      );
    case "stopped":
      return (
        <>
          <b>stopped</b> — {e.detail}. {e.tax_summary}
        </>
      );
    default:
      return null;
  }
}

/** Start or stop the engine. The same thing the `p` shortcut does. */
export function EngineButton() {
  const { status, toggleEngine } = useApp();
  const running = status?.engine_running === true;
  return (
    <button
      type="button"
      className={`btn ${running ? "" : "btn--primary"}`}
      disabled={status?.mode == null}
      onClick={() => void toggleEngine()}
    >
      {running ? "stop engine" : "start engine"} <kbd className="kbd">p</kbd>
    </button>
  );
}
