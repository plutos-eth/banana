/**
 * View 5: the rules.
 *
 * Edits a `StrategyConfig` and writes it to the one `strategy.json` the Lab reads and the
 * sniper will arm from — spec §7.2 makes that identity the point of the product, so this
 * view has no model of its own and no translation step.
 *
 * A live count comes from the same backtest the Lab runs, so the number beside the editor
 * and the number in the funnel cannot disagree. It is the whole reason the light half is
 * required to be fast (PLAN.md D2).
 */

import { useCallback, useEffect, useState } from "react";
import { api, hasBackend, type Condition, type EntryFilter, type PassCount } from "../ipc";
import { count } from "../format";
import { useApp } from "../store";
import { Empty } from "../components/Empty";

/** The flat AND the first UI exposes. The tree underneath is `All`/`Any`/`Not` already. */
function conditions(f: EntryFilter): Condition[] {
  if (f.op === "cond") return [f.of];
  if (f.op === "not") return conditions(f.of);
  return f.of.flatMap(conditions);
}

function asAllOf(cs: Condition[]): EntryFilter {
  return { op: "all", of: cs.map((c) => ({ op: "cond", of: c }) as EntryFilter) };
}

export function Rules() {
  const { draft, dirty, setDraft, saveDraft, revertDraft, setError } = useApp();
  const [live, setLive] = useState<PassCount | null>(null);

  const recount = useCallback(async () => {
    if (!hasBackend() || !draft) return;
    try {
      setLive(await api.passCount(draft));
    } catch (e) {
      setError(e);
      setLive(null);
    }
  }, [draft, setError]);

  useEffect(() => {
    const t = setTimeout(() => void recount(), 150);
    return () => clearTimeout(t);
  }, [recount]);

  if (!hasBackend() || !draft) {
    return <Empty title="No backend" note="Run the desktop app to edit rules." />;
  }

  const cs = conditions(draft.entry_filter);
  const update = (i: number, next: Condition) => {
    const copy = [...cs];
    copy[i] = next;
    setDraft({ ...draft, entry_filter: asAllOf(copy) });
  };
  const remove = (i: number) =>
    setDraft({ ...draft, entry_filter: asAllOf(cs.filter((_, j) => j !== i)) });
  const add = (c: Condition) => setDraft({ ...draft, entry_filter: asAllOf([...cs, c]) });

  return (
    <div className="view view--scroll">
      <div className="toolbar">
        <span className="toolbar__title">Rules</span>
        <span className="toolbar__spacer" />
        {live && (
          <span className="toolbar__note mono">
            {count(live.passed)} of {count(live.matured)} matured pass · {live.query_ms} ms
          </span>
        )}
        <button type="button" className="btn" onClick={revertDraft} disabled={!dirty}>
          revert
        </button>
        <button
          type="button"
          className="btn btn--primary"
          onClick={() => void saveDraft()}
          disabled={!dirty}
        >
          {dirty ? "save strategy" : "saved"}
        </button>
      </div>

      <p className="note">
        A launch passes when <em>every</em> rule below passes. Saving writes
        <span className="mono"> strategy.json</span> — the same file the Lab reads and the
        sniper will arm from, with no translation step.
      </p>

      <section className="panel">
        <h2 className="panel__title">Entry rules</h2>
        {cs.length === 0 && (
          <p className="note">No rules. An empty filter passes every launch.</p>
        )}
        {cs.map((c, i) => (
          <div key={`${c.kind}-${i}`} className="rule">
            <span className="rule__name mono">{c.kind}</span>
            <ConditionEditor c={c} onChange={(n) => update(i, n)} />
            <button type="button" className="btn btn--quiet" onClick={() => remove(i)}>
              remove
            </button>
          </div>
        ))}
        <div className="rule rule--add">
          <select
            className="input input--sm"
            value=""
            onChange={(e) => {
              const k = e.target.value;
              if (k) add(blank(k));
            }}
          >
            <option value="">add a rule…</option>
            {ADDABLE.map((k) => (
              <option key={k} value={k}>
                {k}
              </option>
            ))}
          </select>
        </div>
      </section>

      <section className="panel">
        <h2 className="panel__title">Entry model</h2>
        <p className="note">
          The opening tax starts at 99% and decays to zero over about three seconds. Buying
          at block one hands almost the whole spend to the creator, so the edge is when,
          not how fast.
        </p>
        <Stepper
          label="max entry tax"
          suffix="bps"
          value={draft.entry_model.max_tax_bps}
          step={25}
          min={0}
          max={9900}
          onChange={(v) =>
            setDraft({ ...draft, entry_model: { ...draft.entry_model, max_tax_bps: v } })
          }
        />
        <Stepper
          label="slippage"
          suffix="bps"
          value={draft.entry_model.slippage_bps}
          step={25}
          min={0}
          max={5000}
          onChange={(v) =>
            setDraft({ ...draft, entry_model: { ...draft.entry_model, slippage_bps: v } })
          }
        />
        <Stepper
          label="give up waiting after"
          suffix="ms"
          value={draft.entry_model.max_wait_ms}
          step={1000}
          min={0}
          max={120000}
          onChange={(v) =>
            setDraft({ ...draft, entry_model: { ...draft.entry_model, max_wait_ms: v } })
          }
        />
      </section>

      <section className="panel">
        <h2 className="panel__title">Saved file</h2>
        <pre className="json mono">{JSON.stringify(draft, null, 2)}</pre>
      </section>
    </div>
  );
}

const ADDABLE = [
  "require_twitter",
  "require_website",
  "require_telegram",
  "require_any_social",
  "dev_buy_bps",
  "max_creator_tax_bps",
  "max_exempt_wallets",
  "max_fingerprint_twins",
  "max_deployer_launches",
];

function blank(kind: string): Condition {
  switch (kind) {
    case "dev_buy_bps":
      return { kind: "dev_buy_bps", min: 100, max: 600 };
    case "max_creator_tax_bps":
      return { kind: "max_creator_tax_bps", bps: 200 };
    case "max_exempt_wallets":
      return { kind: "max_exempt_wallets", max: 2 };
    case "max_fingerprint_twins":
      return { kind: "max_fingerprint_twins", max: 1 };
    case "max_deployer_launches":
      return { kind: "max_deployer_launches", max: 3 };
    default:
      return { kind } as Condition;
  }
}

function ConditionEditor({ c, onChange }: { c: Condition; onChange: (c: Condition) => void }) {
  switch (c.kind) {
    case "dev_buy_bps":
      return (
        <span className="rule__fields">
          <Stepper
            label="min"
            suffix="bps"
            value={c.min ?? 0}
            step={25}
            min={0}
            max={10000}
            onChange={(v) => onChange({ ...c, min: v })}
          />
          <Stepper
            label="max"
            suffix="bps"
            value={c.max ?? 10000}
            step={25}
            min={0}
            max={10000}
            onChange={(v) => onChange({ ...c, max: v })}
          />
        </span>
      );
    case "max_creator_tax_bps":
      return (
        <Stepper
          label="ceiling"
          suffix="bps"
          value={c.bps}
          step={25}
          min={0}
          max={10000}
          onChange={(v) => onChange({ ...c, bps: v })}
        />
      );
    case "max_exempt_wallets":
    case "max_fingerprint_twins":
    case "max_deployer_launches":
      return (
        <Stepper
          label="ceiling"
          value={c.max}
          step={1}
          min={0}
          max={99}
          onChange={(v) => onChange({ ...c, max: v })}
        />
      );
    default:
      return <span className="rule__fields note">must be declared at launch</span>;
  }
}

function Stepper({
  label,
  suffix,
  value,
  step,
  min,
  max,
  onChange,
}: {
  label: string;
  suffix?: string;
  value: number;
  step: number;
  min: number;
  max: number;
  onChange: (v: number) => void;
}) {
  const clamp = (v: number) => Math.min(max, Math.max(min, v));
  return (
    <span className="stepper">
      <span className="stepper__label">{label}</span>
      <button type="button" className="btn btn--quiet" onClick={() => onChange(clamp(value - step))}>
        −
      </button>
      <input
        className="input input--num mono"
        value={value}
        inputMode="numeric"
        onChange={(e) => {
          const v = Number(e.target.value.replace(/[^0-9]/g, ""));
          if (Number.isFinite(v)) onChange(clamp(v));
        }}
      />
      <button type="button" className="btn btn--quiet" onClick={() => onChange(clamp(value + step))}>
        +
      </button>
      {suffix && <span className="stepper__suffix">{suffix}</span>}
    </span>
  );
}
