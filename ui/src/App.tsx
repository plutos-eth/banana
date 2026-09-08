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
import { events, hasBackend } from "./ipc";
import { count, duration } from "./format";
import { useApp, VIEWS } from "./store";
import { ModeBadge } from "./components/ModeBadge";
import { TitleBar } from "./components/TitleBar";
import { Feed } from "./views/Feed";
import { Positions } from "./views/Positions";
import { Lab } from "./views/Lab";
import { IndexView } from "./views/IndexView";
import { StatusView } from "./views/StatusView";
import { Onboarding } from "./views/Onboarding";
import { ChooseMode } from "./views/ChooseMode";

export function App() {
  const {
    view,
    setView,
    status,
    refreshStatus,
    loadStrategy,
    error,
    setError,
    indexProgress,
    setIndexProgress,
    setIndexDone,
    pushEngineEvent,
    toggleEngine,
  } = useApp();

  useEffect(() => {
    void refreshStatus();
    void loadStrategy();
  }, [refreshStatus, loadStrategy]);

  // A live session darkens the whole shell (§3.2). The class goes on <body> so every
  // surface follows from tokens.css without a single component knowing which mode it is
  // in — and so the signal is peripheral, visible to someone who is not looking at the
  // badge in the corner.
  useEffect(() => {
    document.body.classList.toggle("is-live-session", status?.can_spend === true);
  }, [status?.can_spend]);

  // The index runs in the backend and outlives any view (spec §4.2), so the shell is what
  // listens. Subscribing here rather than in the Index view is what lets the progress bar
  // survive a view change, and what makes a running index visible from every screen.
  useEffect(() => {
    if (!hasBackend()) return;
    const unlisten = [
      events.indexProgress(setIndexProgress),
      events.indexDone((d) => {
        setIndexDone(d);
        void refreshStatus();
      }),
    ];
    return () => {
      unlisten.forEach((p) => void p.then((f) => f()));
    };
  }, [setIndexProgress, setIndexDone, refreshStatus]);

  // The engine outlives every view, so the shell is what listens (spec §4.2). Switching
  // screens mid-session must not lose what happened while you were on another one.
  useEffect(() => {
    if (!hasBackend()) return;
    const unlisten = [
      events.engine(pushEngineEvent),
      events.engineStopped(() => {
        void refreshStatus();
      }),
    ];
    return () => {
      unlisten.forEach((p) => void p.then((f) => f()));
    };
  }, [pushEngineEvent, refreshStatus]);

  // Numbers switch views; `p` starts and stops the engine. The rest is handled by
  // whichever view owns the shortcut.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.target instanceof HTMLElement && ["INPUT", "TEXTAREA"].includes(e.target.tagName)) {
        return;
      }
      // Not while a decision screen is up: there is nothing to switch to yet.
      if (useApp.getState().status?.mode == null) return;
      if (e.key === "p") {
        e.preventDefault();
        void toggleEngine();
        return;
      }
      const hit = VIEWS.find((v) => v.key === e.key);
      if (hit) {
        e.preventDefault();
        setView(hit.id);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [setView, toggleEngine]);

  // The mode comes first: nothing else should be reachable before the user has said
  // whether this session can spend money.
  const choosing = status !== null && status.mode === null;

  // Then, on a first run, the questionnaire — which fills the same `StrategyConfig` the
  // rest of the app uses and introduces no new entity (spec §8).
  const onboarding = status !== null && !status.has_saved_strategy;

  return (
    <div className="app">
      <TitleBar />
      <header className="topbar">
        {/* One letter of the wordmark in the key colour, and a cursor after it. The
            application is a terminal; this is the only place it says so out loud. */}
        <span className="topbar__brand">
          quarre<em>l</em>
        </span>
        <span className="topbar__cursor" aria-hidden="true">
          ▋
        </span>
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
              {view === "status" && <StatusView />}
            </>
          )}
        </main>
      </div>

      {/* The error and the live strip sit side by side rather than taking turns. An
          error is sticky until dismissed, so letting it win the whole bar means a
          five-minute-old failure hides a running index and a running engine — which is
          how you end up staring at a screen that looks idle while it is working. */}
      <footer className="statusbar">
        {/* The sentence reads from the left edge, like a terminal. Only an error takes
            that position from it — and the live strip keeps the right, so a five-minute-old
            failure can never hide a running index or a running engine. */}
        {error ? (
          <button type="button" className="statusbar__error" onClick={() => setError(null)}>
            {error} — click to dismiss
          </button>
        ) : (
          <span className="statusbar__note">{status?.engine ?? "starting"}</span>
        )}
        <span className="topbar__spacer" />
        {indexProgress ? <IndexPulse /> : status?.engine_running ? <EnginePulse /> : null}
      </footer>
    </div>
  );
}

/**
 * The running index, in the one strip that is on screen whatever view is showing.
 *
 * Clicking it goes to the Index view, which has the phase detail. The percentage is the
 * indexer's own four-phase weighted figure, not a spinner: a bar that moves at a rate
 * unrelated to the work left is worse than no bar, because it is read as a promise.
 */
function IndexPulse() {
  const { indexProgress: p, setView } = useApp();
  if (!p) return null;
  return (
    <button type="button" className="statusbar__index" onClick={() => setView("index")}>
      <span className="statusbar__index-label">indexing · {p.phase}</span>
      <span className="bar bar--slim">
        <span className="bar__fill" style={{ width: `${p.percent_x10 / 10}%` }} />
      </span>
      <span className="mono statusbar__index-figures">
        {(p.percent_x10 / 10).toFixed(1)}% · {count(p.rows_written)} rows
        {p.eta_secs !== null && ` · eta ${duration(p.eta_secs)}`}
      </span>
    </button>
  );
}

/**
 * The running engine, in the strip that is on screen whatever view is showing.
 *
 * Shows the last health pulse, because a feed that has gone quiet because the endpoint is
 * refusing looks exactly like a feed that is quiet because nobody is launching anything —
 * and only one of those is worth doing something about.
 */
function EnginePulse() {
  const { pulse, activity, setView } = useApp();
  const last = activity.find((e) => e.kind !== "seen");
  return (
    <button type="button" className="statusbar__index" onClick={() => setView("positions")}>
      <span className="statusbar__index-label">
        <span className="pulse pulse--live" /> engine running
      </span>
      <span className="mono statusbar__index-figures">
        {pulse
          ? `head ${count(pulse.head)} · ${pulse.behind_blocks} behind · ${pulse.latency_ms} ms · ${count(pulse.watched)} launches seen`
          : "waiting for the first sweep"}
        {last?.kind === "trouble" && " · feed trouble"}
      </span>
    </button>
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
