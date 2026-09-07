/**
 * View 4: the index.
 *
 * The progress bar is the real four-phase weighted figure the indexer emits, not a
 * spinner and not a per-request count — spec §6.2 says a two-phase weighting would lie,
 * and the phases differ in cost by orders of magnitude (PLAN.md F1, D4).
 *
 * Resume state is shown rather than implied: each phase's last block against its target,
 * so a user who killed a run can see exactly what a restart will and will not redo.
 */

import { useCallback, useEffect, useState } from "react";
import { api, hasBackend, type IndexStatus } from "../ipc";
import { bytes, count, duration, hours } from "../format";
import { useApp } from "../store";
import { Empty } from "../components/Empty";

export function IndexView() {
  // Progress and the finished-run banner live in the store, not here: the run outlives
  // this component, and state kept here would vanish every time the user looked at
  // another view. The shell subscribes; this view reads.
  const { setError, indexProgress: progress, indexDone: done, setIndexDone } = useApp();
  const [status, setStatus] = useState<IndexStatus | null>(null);
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");

  const load = useCallback(async () => {
    if (!hasBackend()) return;
    try {
      setStatus(await api.indexStatus());
    } catch (e) {
      setError(e);
    }
  }, [setError]);

  useEffect(() => {
    void load();
  }, [load]);

  // Coverage is re-read when a run finishes, wherever the user was standing when it did.
  useEffect(() => {
    if (done) void load();
  }, [done, load]);

  if (!hasBackend()) {
    return <Empty title="No backend" note="Run the desktop app to index." />;
  }

  const running = progress !== null || status?.indexing === true;

  return (
    <div className="view view--scroll">
      <div className="toolbar">
        <span className="toolbar__title">Index</span>
        <span className="toolbar__spacer" />
        <input
          className="input input--sm"
          placeholder="from block"
          value={from}
          onChange={(e) => setFrom(e.target.value)}
          spellCheck={false}
        />
        <input
          className="input input--sm"
          placeholder="to block"
          value={to}
          onChange={(e) => setTo(e.target.value)}
          spellCheck={false}
        />
        <button
          type="button"
          className="btn btn--primary"
          disabled={running}
          onClick={() => {
            setIndexDone(null);
            const f = from.trim() ? Number(from) : null;
            const t = to.trim() ? Number(to) : null;
            api.startIndex(f, t).catch((e) => setError(e));
          }}
        >
          {running ? "indexing…" : from || to ? "index range" : "index last 24 h"}
        </button>
      </div>

      <p className="note">
        Leave both boxes empty for the last 24 hours, which is ~856,500 blocks. Resumable:
        an interrupted run picks up from its last checkpoint, and re-indexing a covered
        range writes nothing.
      </p>

      {progress && (
        <section className="panel">
          <h2 className="panel__title">{progress.phase}</h2>
          <div className="bar">
            <span className="bar__fill" style={{ width: `${progress.percent_x10 / 10}%` }} />
          </div>
          <div className="note mono">
            {(progress.percent_x10 / 10).toFixed(1)}% overall ·{" "}
            {count(progress.units_done)} / {count(progress.units_total)} ·{" "}
            {count(progress.rows_written)} rows
            {progress.eta_secs !== null && ` · eta ${duration(progress.eta_secs)}`}
          </div>
          <p className="note">
            The percentage weights the four phases by what they actually cost, so it moves
            unevenly on purpose. The trade scan is the long one.
          </p>
        </section>
      )}

      {done && (
        <div className={`banner ${done.ok ? "banner--info" : "banner--error"}`}>
          {done.ok
            ? `Finished in ${duration(done.elapsed_secs)}: ${count(done.launches)} launch rows, ${count(done.trades)} trade rows.`
            : `Index stopped: ${done.error}`}
        </div>
      )}

      <section className="panel">
        <h2 className="panel__title">Coverage</h2>
        {!status?.store.exists ? (
          <p className="note">No store yet. The first index creates it.</p>
        ) : (
          <table className="kv">
            <tbody>
              <tr>
                <th>window</th>
                <td className="mono">
                  {count(status.store.from_block)} … {count(status.store.to_block)} (
                  {hours(status.store.hours_x10)})
                </td>
              </tr>
              <tr>
                <th>launches</th>
                <td className="mono">{count(status.store.launches)}</td>
              </tr>
              <tr>
                <th>trades</th>
                <td className="mono">{count(status.store.trades)}</td>
              </tr>
              <tr>
                <th>outcomes</th>
                <td className="mono">{count(status.store.outcomes)}</td>
              </tr>
              <tr>
                <th>unreadable launches</th>
                <td className="mono">{count(status.undecodable)}</td>
              </tr>
              <tr>
                <th>launched via a bundler</th>
                <td className="mono">{count(status.bundled)}</td>
              </tr>
              <tr>
                <th>migrated</th>
                <td className="mono">{count(status.migrated)}</td>
              </tr>
              <tr>
                <th>database</th>
                <td className="mono">{bytes(status.store.bytes)}</td>
              </tr>
            </tbody>
          </table>
        )}
      </section>

      {status && status.phases.length > 0 && (
        <section className="panel">
          <h2 className="panel__title">Resume state</h2>
          <p className="note">
            Where each phase got to. A restart continues from the last block, not from the
            beginning. Row totals are in Coverage above: the checkpoint records what the
            last <em>run</em> wrote, which is zero for a phase that was already finished,
            and showing that beside "complete" read as an empty store.
          </p>
          <table className="kv">
            <tbody>
              {status.phases.map((p) => (
                <tr key={p.phase}>
                  <th>{p.phase}</th>
                  <td className="mono">
                    {count(p.last_block)} / {count(p.target_block)}{" "}
                    {p.complete ? "complete" : "incomplete"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      )}
    </div>
  );
}
