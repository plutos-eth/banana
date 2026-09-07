/**
 * The shell: which view is showing, and the two things that must be on screen whatever
 * the answer is.
 *
 * Spec §3.2 and §8 view 6: the operating mode is visible at all times and never
 * ambiguous. It lives in the top bar, outside the view router, so there is no route that
 * can render without it.
 *
 * Keyboard map (§8): `p` start/stop, `f` passing-only, `/` search, `esc` close drawer.
 * `p` has nothing to start in this build and says so rather than doing nothing silently.
 */

import { useEffect } from "react";
import { hasBackend } from "./ipc";
import { useApp, VIEWS } from "./store";
import { ModeBadge } from "./components/ModeBadge";
import { Feed } from "./views/Feed";
import { Positions } from "./views/Positions";
import { Lab } from "./views/Lab";
import { IndexView } from "./views/IndexView";
import { Rules } from "./views/Rules";
import { StatusView } from "./views/StatusView";
import { Onboarding } from "./views/Onboarding";
import { ChooseMode } from "./views/ChooseMode";

export function App() {
  const { view, setView, status, refreshStatus, loadStrategy, error, setError } = useApp();

  useEffect(() => {
    void refreshStatus();
    void loadStrategy();
  }, [refreshStatus, loadStrategy]);

  // Numbers switch views; the rest is handled by whichever view owns the shortcut.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.target instanceof HTMLElement && ["INPUT", "TEXTAREA"].includes(e.target.tagName)) {
        return;
      }
      // Not while a decision screen is up: there is nothing to switch to yet.
      if (useApp.getState().status?.mode == null) return;
      const hit = VIEWS.find((v) => v.key === e.key);
      if (hit) {
        e.preventDefault();
        setView(hit.id);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [setView]);

  // The mode comes first: nothing else should be reachable before the user has said
  // whether this session can spend money.
  const choosing = status !== null && status.mode === null;

  // Then, on a first run, the questionnaire — which fills the same `StrategyConfig` the
  // rest of the app uses and introduces no new entity (spec §8).
  const onboarding = status !== null && !status.has_saved_strategy;

  return (
    <div className="app">
      <header className="topbar">
        <span className="topbar__brand">quarrel</span>
        <ModeBadge status={status} />
        <span className="topbar__spacer" />
        {!hasBackend() && (
          <span className="topbar__note">
            no backend — layout preview only, nothing here is real data
          </span>
        )}
        {status && (
          <span className="topbar__note mono">
            chain {status.chain_id} · {status.data_dir}
          </span>
        )}
      </header>

      <div className="app__body">
        {/* The navigation is hidden until the session has a mode and a strategy. A
            sidebar you can click during a decision screen reads as a decision you can
            skip, and neither of these is skippable. */}
        {!choosing && !onboarding && (
          <nav className="sidebar" aria-label="views">
            {VIEWS.map((v) => (
              <button
                key={v.id}
                type="button"
                className={`sidebar__item${view === v.id ? " is-active" : ""}`}
                onClick={() => setView(v.id)}
              >
                <span>{v.label}</span>
                <kbd className="kbd">{v.key}</kbd>
              </button>
            ))}
            <div className="sidebar__spacer" />
            <ShortcutLegend />
          </nav>
        )}

        <main className="content">
          {choosing ? (
            <ChooseMode />
          ) : onboarding ? (
            <Onboarding />
          ) : (
            <>
              {view === "feed" && <Feed />}
              {view === "positions" && <Positions />}
              {view === "lab" && <Lab />}
              {view === "index" && <IndexView />}
              {view === "rules" && <Rules />}
              {view === "status" && <StatusView />}
            </>
          )}
        </main>
      </div>

      <footer className="statusbar">
        {error ? (
          <button type="button" className="statusbar__error" onClick={() => setError(null)}>
            {error} — click to dismiss
          </button>
        ) : (
          <span className="statusbar__note">
            {status?.engine ?? "starting"}
          </span>
        )}
      </footer>
    </div>
  );
}

function ShortcutLegend() {
  return (
    <div className="legend">
      <div className="legend__row">
        <kbd className="kbd">p</kbd> <span>start / stop</span>
      </div>
      <div className="legend__row">
        <kbd className="kbd">f</kbd> <span>passing only</span>
      </div>
      <div className="legend__row">
        <kbd className="kbd">/</kbd> <span>search</span>
      </div>
      <div className="legend__row">
        <kbd className="kbd">esc</kbd> <span>close drawer</span>
      </div>
    </div>
  );
}
