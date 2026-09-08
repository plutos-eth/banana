/**
 * View 3: the Strategy Lab.
 *
 * This is where the honesty guards have to survive contact with a screen, so the layout
 * follows them rather than the other way round:
 *
 * * **The funnel is first**, not a chart at the bottom. Spec §5.5 calls it mandatory, and
 *   a denominator below the fold is a denominator nobody reads.
 * * **The headline is the fixed hold** (§5.4). The peak is a separate panel, under its own
 *   heading, carrying the label the backend sends with it. This file contains no word
 *   from the "profit / return / earned" family, and neither does anything it renders.
 * * **Below thirty passing tokens there is nothing to render.** The backend sends a
 *   different variant, with no percentage field in it at all, so the numbers cannot be
 *   shown by accident — there is no value here to hide.
 *
 * The rule editor is the second half of this view rather than a screen of its own. A
 * result and the rules that produced it are one thought, and the count beside the editor
 * comes from the same backtest as the funnel above it, so the two cannot disagree.
 */

import { useCallback, useEffect, useState } from "react";
import { api, hasBackend, type BacktestResult } from "../ipc";
import { count, duration, hours, multiple } from "../format";
import { useApp } from "../store";
import { Empty } from "../components/Empty";
import { Rules } from "./Rules";

export function Lab() {
  const { saved, setError } = useApp();
  const [result, setResult] = useState<BacktestResult | null>(null);
  const [running, setRunning] = useState(false);

  const run = useCallback(async () => {
    if (!hasBackend() || !saved) return;
    setRunning(true);
    try {
      setResult(await api.backtest(saved));
    } catch (e) {
      setError(e);
      setResult(null);
    } finally {
      setRunning(false);
    }
  }, [saved, setError]);

  useEffect(() => {
    void run();
  }, [run]);

  if (!hasBackend()) {
    return <Empty title="No backend" note="Run the desktop app to backtest." />;
  }

  return (
    <div className="view view--scroll">
      <div className="toolbar">
        <span className="toolbar__title">Strategy Lab</span>
        <span className="toolbar__spacer" />
        {result && (
          <span className="toolbar__note mono">
            {hours(result.window.hours_x10)} window · query {result.query_ms} ms
          </span>
        )}
        <button type="button" className="btn" onClick={() => void run()} disabled={running}>
          {running ? "running…" : "re-run"}
        </button>
      </div>

      {!result ? (
        <Empty
          title={running ? "Running" : "No result"}
          note="Index a window from the Index view, then tune the rules below."
        />
      ) : (
        <>
          {/* §5.5: stated plainly, in the primary reading path, not in a footer. */}
          <div className="banner banner--warn">{result.regime_warning}</div>

          <Funnel result={result} />
          <Results result={result} />
        </>
      )}

      {/* The rules that produced everything above. One screen, because a result and the
          rule that narrowed it are one thought — and because the count in the editor and
          the count in the funnel come from the same backtest and must not be read apart. */}
      <Rules />
    </div>
  );
}

function Funnel({ result }: { result: BacktestResult }) {
  const first = result.funnel.stages[0]?.remaining ?? 0;
  return (
    <section className="panel">
      <h2 className="panel__title">Funnel</h2>
      <p className="note">
        Where the universe went. Maturity cutoff {result.window.maturity_cutoff_hours} h;
        a launch is mature when the window kept watching it that long, whatever it did.
      </p>
      <table className="funnel">
        <tbody>
          {result.funnel.stages.map((s) => (
            <tr key={s.id}>
              <th className="funnel__count mono">{count(s.remaining)}</th>
              <td className="funnel__bar">
                {/* The fill sits in a track, so a stage that kept under a percent still
                    reads as a nearly-empty bar rather than as a missing one. */}
                <span className="funnel__track">
                  <span
                    className="funnel__fill"
                    style={{ width: `${first > 0 ? (s.remaining / first) * 100 : 0}%` }}
                  />
                </span>
              </td>
              <td className="funnel__label">
                {s.label}
                {s.of && s.of !== "all_launches" && (
                  <span className="funnel__of"> (of {s.of})</span>
                )}
              </td>
              <td className="funnel__removed mono">{s.removed > 0 ? `−${count(s.removed)}` : ""}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {result.deployer_depth_applied && (
        <p className="note">
          This strategy reads a deployer feature, so launches without enough visible
          deployer history are excluded — that is the stage above, and its cost is real.
        </p>
      )}
    </section>
  );
}

function Results({ result }: { result: BacktestResult }) {
  const r = result.results;

  // Spec §5.5: not a warning beside a number. There is no number.
  if (r.status === "insufficient_sample") {
    return (
      <section className="panel">
        <h2 className="panel__title">Result</h2>
        <div className="gate">
          <div className="gate__headline">{r.message}</div>
          <p className="note">
            {r.required} are needed before a hit rate or a distribution means anything.
            Loosen a rule, or index a wider window.
          </p>
        </div>
      </section>
    );
  }

  return (
    <>
      <section className="panel">
        <h2 className="panel__title">Result</h2>
        <div className="headline">
          <div className="headline__value mono">{(r.hit_rate_bps / 100).toFixed(1)}%</div>
          <div className="headline__label">
            {r.target}
            <br />
            <span className="note">
              {count(r.hits)} of {count(r.measured_over)} · {count(r.migrations)} migrated
            </span>
          </div>
        </div>
        {r.target_is_peak_based && (
          <div className="banner banner--warn">
            This target counts a peak. Nobody sells at the peak; it lasts seconds and is
            unidentifiable in the moment. The fixed holds below are what a rule could
            actually have done.
          </div>
        )}
      </section>

      <section className="panel">
        <h2 className="panel__title">Held for a fixed time</h2>
        <p className="note">
          The number a simple, executable rule would have produced. Every panel states the
          holding assumption it rests on.
        </p>
        {[r.hold_5m, r.hold_30m].map((h) => (
          <div key={h.assumption} className="hold">
            <div className="hold__assumption">{h.assumption}</div>
            <Percentiles p={h.multiple} />
            <div className="note mono">
              above entry {count(h.above_entry)} · below entry {count(h.below_entry)} ·
              measured over {count(h.measured_over)}
            </div>
          </div>
        ))}
      </section>

      <section className="panel">
        {/* §5.4: the label travels with the number, from the backend. */}
        <h2 className="panel__title">Peak</h2>
        <p className="note">{r.peak.label}</p>
        <Percentiles p={r.peak.multiple} />
        <div className="note mono">
          never traded above entry {count(r.peak.never_above_entry)} of{" "}
          {count(r.peak.measured_over)}
        </div>
      </section>
    </>
  );
}

function Percentiles({ p }: { p: { p10: number; p25: number; p50: number; p75: number; p90: number } }) {
  const cells: [string, number][] = [
    ["p10", p.p10],
    ["p25", p.p25],
    ["p50", p.p50],
    ["p75", p.p75],
    ["p90", p.p90],
  ];
  return (
    <div className="pcts">
      {cells.map(([k, v]) => (
        <div key={k} className="pcts__cell">
          <span className="pcts__key">{k}</span>
          <span className="pcts__val mono">{multiple(v)}</span>
        </div>
      ))}
    </div>
  );
}

/** Re-exported for the Rules view's live count, which shows the same window figure. */
export const labWindow = (r: BacktestResult) => duration(r.window.hours_x10 * 360);
