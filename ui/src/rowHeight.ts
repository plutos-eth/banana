/**
 * The feed row height, read from tokens.css rather than written in a component.
 *
 * PLAN.md F8. A virtualiser needs the row height as a number, which is the one place a
 * pixel value naturally leaks back into TypeScript and quietly desynchronises from the
 * CSS — the rows would then overlap or leave gaps after a restyle, and the restyle is
 * supposed to be an edit to one file.
 *
 * So the number comes from the same custom property the CSS uses. Changing `--h-row`
 * changes both.
 */

const FALLBACK = 26;

let cached: number | null = null;

export function rowHeight(): number {
  if (cached !== null) return cached;
  cached = readToken("--h-row") ?? FALLBACK;
  return cached;
}

/** Exposed for tests and for a future density toggle. */
export function readToken(name: string): number | null {
  if (typeof window === "undefined" || !document.documentElement) return null;
  const raw = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const px = Number.parseFloat(raw);
  return Number.isFinite(px) && px > 0 ? px : null;
}

/** Called when the token could have changed, e.g. after a theme swap. */
export function forgetRowHeight(): void {
  cached = null;
}
