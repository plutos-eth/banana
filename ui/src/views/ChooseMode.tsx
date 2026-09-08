/**
 * The startup screen: TEST or LIVE.
 *
 * Shown before anything else, once per run. This is the whole mode decision — it replaces
 * the launch flag and the arm phrase that came before it, because two ceremonies for one
 * choice was confusing without being safer.
 *
 * What makes TEST safe is not this screen. A test session holds no key and cannot produce
 * a signature at all, so it could not spend even if this screen were bypassed. What this
 * screen is for is making sure nobody reaches LIVE without seeing which wallet is about to
 * be spent from, how much is in it, and what the limits are.
 *
 * The choice is fixed for the life of the process. Changing it means restarting.
 */

import { useState } from "react";
import { api, type Mode } from "../ipc";
import { useApp } from "../store";

export function ChooseMode() {
  const { refreshStatus, setError } = useApp();
  const [pending, setPending] = useState<Mode | null>(null);
  const [confirming, setConfirming] = useState(false);

  const choose = (mode: Mode) => {
    setPending(mode);
    api
      .chooseMode(mode)
      .then(() => void refreshStatus())
      .catch((e) => {
        setError(e);
        setPending(null);
        setConfirming(false);
      });
  };

  if (confirming) {
    return <ConfirmLive onBack={() => setConfirming(false)} onConfirm={() => choose("live")} />;
  }

  return (
    <div className="chooser">
      <h1 className="chooser__title">banana</h1>
      <p className="chooser__note">
        How should this session run? The choice holds until you restart.
      </p>

      <div className="chooser__options">
        <button
          type="button"
          className="chooser__option"
          disabled={pending !== null}
          onClick={() => choose("test")}
        >
          <span className="chooser__label">TEST</span>
          <span className="chooser__body">
            Watch, filter and backtest. No key is loaded, so nothing can be signed even by
            mistake. Everything else works exactly as it does live.
          </span>
        </button>

        <button
          type="button"
          className="chooser__option chooser__option--live"
          disabled={pending !== null}
          onClick={() => setConfirming(true)}
        >
          <span className="chooser__label">LIVE</span>
          <span className="chooser__body">
            Real money. Entries are signed and sent, inside the session budget. Needs a
            private key saved in Settings.
          </span>
        </button>
      </div>

      {pending && <p className="chooser__note">starting in {pending.toUpperCase()}…</p>}
    </div>
  );
}

/**
 * What used to be behind the arm phrase: the wallet, the balance and the limits.
 *
 * The word was ceremony; this is not. Nobody should start trading without seeing which
 * wallet is about to be spent from and how much of it is at risk.
 */
function ConfirmLive({ onBack, onConfirm }: { onBack: () => void; onConfirm: () => void }) {
  return (
    <div className="chooser">
      <h1 className="chooser__title">Live trading</h1>
      <div className="banner banner--error">
        Real money moves after this point. Entries are signed and sent automatically,
        against the rules in your saved strategy, until the session budget is spent.
      </div>

      <p className="chooser__note">
        The wallet, its balance and the limits that will be enforced are shown on the
        Status view once the session starts. Nothing fires until the engine is running, and
        every entry is refused unless it passes both the filter and the money guards.
      </p>

      <div className="chooser__options chooser__options--row">
        <button type="button" className="btn" onClick={onBack}>
          back
        </button>
        <button type="button" className="btn btn--primary" onClick={onConfirm}>
          start in LIVE
        </button>
      </div>
    </div>
  );
}
