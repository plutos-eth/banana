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
import { api, hasBackend, type Status } from "../ipc";
import { bytes, count, hours } from "../format";
import { useApp } from "../store";
import { Empty } from "../components/Empty";

export function StatusView() {
  const { status, refreshStatus } = useApp();

  // One source, polled: a second fetch into local state was a duplicate that could
  // disagree with the badge in the top bar.
  useEffect(() => {
    void refreshStatus();
    const t = setInterval(() => void refreshStatus(), 4000);
    return () => clearInterval(t);
  }, [refreshStatus]);

  if (!hasBackend()) {
    return <Empty title="No backend" note="Run the desktop app." />;
  }
  const s = status;
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
          <div className="headline__label">
            {s.engine}
            <br />
            <span className="note">Restart the application to change mode.</span>
          </div>
        </div>
      </section>

      <Wallet status={s} />

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
 * The wallet, set from inside the application rather than from a file the user has to find.
 *
 * The key goes in and never comes back out: what the interface can read is the address it
 * derives. A screenshot of this panel, or a screen share while it is open, leaks nothing
 * that can spend money.
 *
 * It is stored in plaintext at `<data dir>/wallet.key`, which is the honest trade-off and
 * is said here rather than buried in a document — anything that can read that file can
 * take the funds, and the wallet should hold only what you would accept losing.
 */
function Wallet({ status }: { status: Status }) {
  const { refreshStatus, setError } = useApp();
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);

  const save = () => {
    setBusy(true);
    api
      .saveKey(typed)
      .then(() => {
        setTyped("");
        void refreshStatus();
      })
      .catch((e) => setError(e))
      .finally(() => setBusy(false));
  };

  return (
    <section className="panel">
      <h2 className="panel__title">Wallet</h2>

      {status.wallet ? (
        <>
          <table className="kv">
            <tbody>
              <tr>
                <th>address</th>
                <td className="mono is-wrap">{status.wallet}</td>
              </tr>
            </tbody>
          </table>
          <p className="note">
            Stored in plaintext beside the store. Anything that can read that file can take
            the funds, so keep only what you would accept losing in this wallet.
          </p>
          <button
            type="button"
            className="btn btn--quiet"
            onClick={() => {
              api
                .clearKey()
                .then(() => void refreshStatus())
                .catch((e) => setError(e));
            }}
          >
            remove key
          </button>
        </>
      ) : (
        <>
          <p className="note">
            No wallet set. LIVE mode needs one; TEST does not and never loads a key at all.
          </p>
          <div className="rule">
            <input
              className="input mono"
              type="password"
              placeholder="paste a private key (0x… or bare hex)"
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
              spellCheck={false}
              autoComplete="off"
            />
            <button
              type="button"
              className="btn btn--primary"
              disabled={busy || typed.trim().length === 0}
              onClick={save}
            >
              {busy ? "checking…" : "save"}
            </button>
          </div>
          <p className="note">
            It is checked before it is stored, so a mistyped key is refused here rather than
            the first time an order would have fired. It is written in plaintext to
            <span className="mono"> wallet.key</span> beside the store, and it is never read
            back into this window — only the address it derives is.
          </p>
        </>
      )}
    </section>
  );
}
