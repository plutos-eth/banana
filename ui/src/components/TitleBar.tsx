/**
 * The window's own titlebar.
 *
 * The window is undecorated (`decorations: false` in `tauri.conf.json`), so this bar is
 * the whole of it: the drag region, the name, and the three controls. Nothing else in the
 * application can move or close the window.
 *
 * # What this costs
 *
 * Four `core:window` permissions in `crates/app/capabilities/default.json` — dragging,
 * minimise, maximise and close. That is a real widening of the IPC surface for something
 * that is mostly appearance, and it is recorded in PLAN.md D24 rather than left to be
 * discovered in the capability file. It is the smallest set that makes an undecorated
 * window usable: no resize, no position, no visibility, no ability to open another one.
 *
 * # Dragging
 *
 * `data-tauri-drag-region` is handled by the webview itself, so a drag is not a stream of
 * IPC calls. The buttons opt out of it explicitly — without that, clicking one starts a
 * drag instead of pressing it.
 */

import { hasBackend } from "../ipc";

/** Ask the window to do something, if there is a window. */
async function window_(action: "minimize" | "toggleMaximize" | "close") {
  if (!hasBackend()) return;
  // Imported at the call site rather than at the top of the module: the browser preview
  // has no Tauri runtime, and a static import would fail there before anything renders.
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  const w = getCurrentWindow();
  if (action === "minimize") await w.minimize();
  else if (action === "toggleMaximize") await w.toggleMaximize();
  else await w.close();
}

export function TitleBar() {
  return (
    <div className="titlebar" data-tauri-drag-region>
      <span className="titlebar__brand" data-tauri-drag-region>
        <span className="titlebar__dot" aria-hidden="true" />
        <span className="titlebar__name">quarrel</span>
      </span>
      <div className="titlebar__controls">
        <button
          type="button"
          className="winbtn"
          aria-label="minimise"
          onClick={() => void window_("minimize")}
        >
          –
        </button>
        <button
          type="button"
          className="winbtn"
          aria-label="maximise"
          onClick={() => void window_("toggleMaximize")}
        >
          ▢
        </button>
        <button
          type="button"
          className="winbtn winbtn--close"
          aria-label="close"
          onClick={() => void window_("close")}
        >
          ✕
        </button>
      </div>
    </div>
  );
}
