/**
 * How numbers are put on the screen.
 *
 * Two rules, both from the spec rather than from taste:
 *
 * 1. **An unknown is not a zero** (§5.3). Every formatter takes `null | undefined` and
 *    renders `?`, because a launch whose calldata could not be read has an unknown dev
 *    buy, and printing `0.00%` there would be the same lie the `Presence::Unknown` state
 *    exists to prevent.
 * 2. **Peak is never profit** (§5.4). There is no formatter here that puts a `+` or a
 *    currency symbol on a multiple, and none that colours one green.
 */

/** The string shown wherever a value was not readable. */
export const UNKNOWN = "?";

export function bps(v: number | null | undefined): string {
  if (v === null || v === undefined) return UNKNOWN;
  return `${(v / 100).toFixed(2)}%`;
}

/** 20000 -> "2.00x". A multiple of entry, never described as a return. */
export function multiple(v: number | null | undefined): string {
  if (v === null || v === undefined) return UNKNOWN;
  return `${(v / 10000).toFixed(2)}x`;
}

export function count(v: number | null | undefined): string {
  if (v === null || v === undefined) return UNKNOWN;
  return v.toLocaleString("en-GB");
}

/** Whole seconds as a compact duration: 95 -> "1m35s". */
export function duration(secs: number | null | undefined): string {
  if (secs === null || secs === undefined) return UNKNOWN;
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  if (m < 60) return `${m}m${String(secs % 60).padStart(2, "0")}s`;
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, "0")}m`;
}

/**
 * A block timestamp as a local clock time.
 *
 * `null` renders as `~` rather than `?`: the value is not unknown, it is outside the
 * anchored range and therefore not interpolatable (PLAN.md F2). The distinction matters
 * because one is a gap in our reading and the other is a gap in our sampling.
 */
export function clock(ts: number | null | undefined): string {
  if (ts === null || ts === undefined) return "~";
  const d = new Date(ts * 1000);
  return d.toLocaleTimeString("en-GB", { hour12: false });
}

export function bytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}

/** `0x1234…abcd` — enough to recognise, short enough for a dense row. */
export function shortHex(s: string, lead = 6, tail = 4): string {
  if (s.length <= lead + tail + 1) return s;
  return `${s.slice(0, lead)}…${s.slice(-tail)}`;
}

export function hours(x10: number | null | undefined): string {
  if (x10 === null || x10 === undefined) return UNKNOWN;
  return `${(x10 / 10).toFixed(1)} h`;
}
