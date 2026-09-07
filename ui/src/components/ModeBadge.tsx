/**
 * The operating-mode indicator (spec §3.2).
 *
 * Rendered by the shell, outside the view router, so no view can be on screen without it.
 * The word is never abbreviated and never implied by colour alone: a user who is
 * colour-blind, or looking at a screenshot, must still be able to tell whether real money
 * can move.
 */

import type { Status } from "../ipc";

export function ModeBadge({ status }: { status: Status | null }) {
  if (!status) {
    return <span className="mode mode--unknown">CONNECTING</span>;
  }
  // Keyed on `can_spend` rather than on the label, so a mode added later renders as the
  // careful colour until somebody deliberately says otherwise.
  const cls = status.can_spend
    ? "mode--live"
    : status.mode === "dry_run"
      ? "mode--dry"
      : "mode--armed";
  return (
    <span
      className={`mode ${cls}`}
      title={status.engine}
      aria-label={`operating mode: ${status.mode_label}`}
    >
      {status.mode_label}
      {status.indexing && <span className="mode__sub">indexing</span>}
    </span>
  );
}
