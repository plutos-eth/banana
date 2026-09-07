/**
 * View 6: status.
 *
 * Spec §8 asks for a feed-health pulse "so a quiet chain is distinguishable from a dead
 * engine". That distinction is the whole point of the panel, and in this build the honest
 * answer is neither: there is no engine yet. Rather than show a green light that means
 * nothing, the pulse reports what it actually knows — when the store was last written,
 * and how far behind the chain it is — and says plainly that nothing is watching live.
 */

import { useEffect, useState } from "react";
import { api, hasBackend, ARM_PHRASE, type Status } from "../ipc";
import { bytes, count, hours } from "../format";
import { useApp } from "../store";
import { Empty } from "../components/Empty";

export function StatusView() {
  const { status, refreshStatus, setError } = useApp();
  const [gate, setGate] = useState<Status | null>(null);

  useEffect(() => {
    void refreshStatus();
    const t = setInterval(() => void refreshStatus(), 4000);
    return () => clearInterval(t);
  }, [refreshStatus]);

  useEffect(() => {
    if (!hasBackend()) return;
    api.status().then(setGate).catch(setError);
  }, [setError]);

  if (!hasBackend()) {
    return <Empty title="No backend" note="Run the desktop app." />;
  }
  const s = status ?? gate;
  if (!s) return <Empty title="Connecting" note="Reading state from the backend." />;

  return (
    <div className="view view--scroll">
      <div className="toolbar">
        <span className="toolbar__title">Status</span>
      </div>

      <section className="panel">
        <h2 className="panel__title">Mode</h2>
        <div className="headline">
          <div className={`headline__value ${s.can_spend ? "is-live" : "is-dry"}`}>
            {s.mode_label}
          </div>
          <div className="headline__label">{s.engine}</div>
        </div>
      </section>

      <Arm status={s} />

      <section className="panel">
        <h2 className="panel__title">Feed health</h2>
        <div className="pulse-row">
          <span className="pulse pulse--dead" aria-hidden="true" />
          <span>
            Not watching. Nothing is subscribed to the chain in this build, so a quiet
            chain and a dead engine are the same thing here — which is why this says so
            rather than showing a colour.
          </span>
        </div>
      </section>

      <section className="panel">
        <h2 className="panel__title">Session budget</h2>
        <p className="note">
          The money guards are implemented and tested — size per buy, position cap,
          session budget and open-position count — and a dry-run process holds no key at
          all, so they cannot be bypassed by a flag.
        </p>
        <p className="note">
          What is not wired yet is the executor that would spend against them: nothing in
          this build watches the chain or places an order, so no budget has been consumed
          and a figure here would be zero for a reason that has nothing to do with your
          limits.
        </p>
      </section>

      <section className="panel">
        <h2 className="panel__title">Store</h2>
        <table className="kv">
          <tbody>
            <tr>
              <th>path</th>
              <td className="mono is-wrap">{s.store.path}</td>
            </tr>
            <tr>
              <th>window</th>
              <td className="mono">
                {s.store.exists
                  ? `${count(s.store.from_block)} … ${count(s.store.to_block)} (${hours(s.store.hours_x10)})`
                  : "no store yet"}
              </td>
            </tr>
            <tr>
              <th>launches</th>
              <td className="mono">{count(s.store.launches)}</td>
            </tr>
            <tr>
              <th>trades</th>
              <td className="mono">{count(s.store.trades)}</td>
            </tr>
            <tr>
              <th>size</th>
              <td className="mono">{bytes(s.store.bytes)}</td>
            </tr>
            <tr>
              <th>chain</th>
              <td className="mono">{s.chain_id}</td>
            </tr>
            <tr>
              <th>explorer</th>
              <td className="mono is-wrap">{s.explorer}</td>
            </tr>
          </tbody>
        </table>
      </section>
    </div>
  );
}

/**
 * Arming, and the three states it sits between (PLAN.md C7).
 *
 * A dry-run process shows why it cannot be armed rather than a disabled button with no
 * explanation: the answer is "relaunch with --live", and a user who cannot see that will
 * assume the feature is broken.
 */
function Arm({ status }: { status: Status }) {
  const setError = useApp((st) => st.setError);
  const refreshStatus = useApp((st) => st.refreshStatus);
  const [typed, setTyped] = useState("");

  if (status.mode === "dry_run") {
    return (
      <section className="panel">
        <h2 className="panel__title">Arming</h2>
        <p className="note">{status.engine}</p>
        <p className="note">
          <span className="mono">--live</span> is a launch flag, never a button. That is
          what makes "real money moves only behind an explicit flag" a property of this
          process rather than of a check somewhere inside it.
        </p>
      </section>
    );
  }

  if (status.mode === "live_armed") {
    return (
      <section className="panel">
        <h2 className="panel__title">Arming</h2>
        <div className="banner banner--error">
          Armed. Entries will be signed and sent, up to the session budget. Close the
          application to stop.
        </div>
      </section>
    );
  }

  return (
    <section className="panel">
      <h2 className="panel__title">Arming</h2>
      <div className="banner banner--warn">{status.engine}</div>
      <div className="rule">
        <input
          className="input"
          placeholder={`type ${ARM_PHRASE} to arm`}
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          spellCheck={false}
        />
        <button
          type="button"
          className="btn btn--primary"
          onClick={() => {
            api
              .arm(typed)
              .then(() => {
                setTyped("");
                void refreshStatus();
              })
              .catch((e) => setError(e));
          }}
        >
          arm
        </button>
      </div>
      <p className="note">
        The phrase is exact. Anything else leaves the session unarmed, which is the state
        it should stay in unless you meant otherwise.
      </p>
    </section>
  );
}
