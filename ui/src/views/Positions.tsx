/**
 * View 2: positions.
 *
 * There is no engine in this build, so there are no positions. The view says that in
 * words the backend sends, rather than showing an empty table that reads as "you have no
 * positions" — which would be true by accident and misleading on purpose.
 */

import { useEffect, useState } from "react";
import { api, hasBackend, type Positions as P } from "../ipc";
import { useApp } from "../store";
import { Empty } from "../components/Empty";

export function Positions() {
  const setError = useApp((s) => s.setError);
  const [data, setData] = useState<P | null>(null);

  useEffect(() => {
    if (!hasBackend()) return;
    api.positions().then(setData).catch(setError);
  }, [setError]);

  if (!hasBackend()) {
    return <Empty title="No backend" note="Run the desktop app." />;
  }

  return (
    <div className="view view--scroll">
      <div className="toolbar">
        <span className="toolbar__title">Positions</span>
      </div>
      <Empty title="No positions" note={data?.note ?? "reading…"} />
    </div>
  );
}
