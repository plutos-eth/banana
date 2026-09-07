/**
 * An empty view that says why it is empty.
 *
 * A blank panel is indistinguishable from a broken one, and in this app the difference
 * matters: "no launch passes your filter" and "the store has no launches" lead to
 * opposite actions.
 */
export function Empty({ title, note }: { title: string; note: string }) {
  return (
    <div className="empty">
      <div className="empty__title">{title}</div>
      <p className="empty__note">{note}</p>
    </div>
  );
}
