/**
 * First run (spec §8).
 *
 * Seven questions, every one skippable to the baseline of §7.1. The questionnaire
 * **introduces no new entity**: each answer sets a field on the same `StrategyConfig` the
 * Rules view edits and the sniper will arm from, and the last screen shows the file that
 * will be written.
 *
 * It exists because a first-time user faced with a bare rule editor has no way to know
 * which of eleven conditions matter. It is not a wizard that builds something else.
 */

import { useState } from "react";
import { type Condition, type EntryFilter, type StrategyConfig } from "../ipc";
import { useApp } from "../store";

type Answers = {
  aggression: "tight" | "balanced" | "wide";
  requireTwitter: boolean;
  maxTaxBps: number;
  filterFarms: boolean;
  avoidSerialDeployers: boolean;
};

const DEFAULTS: Answers = {
  aggression: "balanced",
  requireTwitter: true,
  maxTaxBps: 300,
  filterFarms: true,
  avoidSerialDeployers: false,
};

/** Turn answers into the one config type. Nothing else is created. */
function build(base: StrategyConfig, a: Answers): StrategyConfig {
  const cs: Condition[] = [];
  if (a.requireTwitter) cs.push({ kind: "require_twitter" });

  const devBuy: Record<Answers["aggression"], [number, number]> = {
    tight: [200, 500],
    balanced: [100, 600],
    wide: [0, 1500],
  };
  const [min, max] = devBuy[a.aggression];
  cs.push({ kind: "dev_buy_bps", min, max });

  const tax: Record<Answers["aggression"], number> = { tight: 100, balanced: 200, wide: 500 };
  cs.push({ kind: "max_creator_tax_bps", bps: tax[a.aggression] });
  cs.push({ kind: "max_exempt_wallets", max: a.aggression === "tight" ? 0 : 2 });

  if (a.filterFarms) cs.push({ kind: "max_fingerprint_twins", max: 1 });
  // A deployer rule costs the C2 history-depth restriction, so it is opt-in and the
  // question says what it costs rather than presenting it as free.
  if (a.avoidSerialDeployers) cs.push({ kind: "max_deployer_launches", max: 3 });

  const entry_filter: EntryFilter = {
    op: "all",
    of: cs.map((c) => ({ op: "cond", of: c }) as EntryFilter),
  };
  return {
    ...base,
    entry_filter,
    entry_model: { ...base.entry_model, max_tax_bps: a.maxTaxBps },
  };
}

export function Onboarding() {
  const { draft, saveDraft, setDraft } = useApp();
  const [a, setA] = useState<Answers>(DEFAULTS);
  const [step, setStep] = useState(0);

  if (!draft) return null;
  const config = build(draft, a);
  const set = <K extends keyof Answers>(k: K, v: Answers[K]) => setA({ ...a, [k]: v });

  const questions = [
    {
      q: "How selective should the filter be?",
      note: "Sets the dev-buy band, the creator-tax ceiling and the bundle limit together. You can change any of them afterwards.",
      body: (
        <Choice
          value={a.aggression}
          onChange={(v) => set("aggression", v as Answers["aggression"])}
          options={[
            ["tight", "Tight — few launches pass"],
            ["balanced", "Balanced — the documented baseline"],
            ["wide", "Wide — most launches pass"],
          ]}
        />
      ),
    },
    {
      q: "Require a Twitter link declared at launch?",
      note: "Read from the launch transaction, not from the token contract, so a link added later does not count. A launch whose transaction could not be read is refused rather than assumed to have none.",
      body: (
        <Choice
          value={a.requireTwitter ? "yes" : "no"}
          onChange={(v) => set("requireTwitter", v === "yes")}
          options={[
            ["yes", "Yes"],
            ["no", "No"],
          ]}
        />
      ),
    },
    {
      q: "How much opening tax will you pay?",
      note: "The tax opens at 99% and decays to zero over about three seconds. Waiting is the whole edge; there is no mempool to race.",
      body: (
        <Choice
          value={String(a.maxTaxBps)}
          onChange={(v) => set("maxTaxBps", Number(v))}
          options={[
            ["100", "1% — wait longest"],
            ["300", "3% — the baseline"],
            ["900", "9% — enter sooner"],
          ]}
        />
      ),
    },
    {
      q: "Filter out launch farms?",
      note: "One operator printing near-identical tokens from many wallets. Detected from what the launch calldata fixes, so it is point-in-time.",
      body: (
        <Choice
          value={a.filterFarms ? "yes" : "no"}
          onChange={(v) => set("filterFarms", v === "yes")}
          options={[
            ["yes", "Yes"],
            ["no", "No"],
          ]}
        />
      ),
    },
    {
      q: "Avoid deployers with a history of launches?",
      note: "This one has a cost: a deployer rule restricts the backtest to launches with enough visible deployer history, which throws away part of the window. The funnel shows how much.",
      body: (
        <Choice
          value={a.avoidSerialDeployers ? "yes" : "no"}
          onChange={(v) => set("avoidSerialDeployers", v === "yes")}
          options={[
            ["no", "No — keep the whole window"],
            ["yes", "Yes — and accept the restriction"],
          ]}
        />
      ),
    },
    {
      q: "Everything below runs in dry run.",
      note: "This build cannot sign a transaction: there is no executor in it and no key is read anywhere. Live trading arrives in a later phase and will need an explicit arming step.",
      body: <div className="mode mode--dry">DRY RUN</div>,
    },
    {
      q: "This is the file that will be saved.",
      note: "One file. The Lab reads it, and the sniper will arm from it with no translation step.",
      body: <pre className="json mono">{JSON.stringify(config, null, 2)}</pre>,
    },
  ];

  const current = questions[Math.min(step, questions.length - 1)]!;
  const last = step === questions.length - 1;
  const finish = () => {
    setDraft(config);
    void saveDraft();
  };

  return (
    <div className="view view--scroll onboarding">
      <div className="onboarding__head">
        <h1 className="onboarding__title">Set up a strategy</h1>
        <span className="note mono">
          {step + 1} / {questions.length}
        </span>
      </div>

      <section className="panel">
        <h2 className="panel__title">{current.q}</h2>
        <p className="note">{current.note}</p>
        {current.body}
      </section>

      <div className="onboarding__nav">
        <button
          type="button"
          className="btn"
          onClick={() => setStep(Math.max(0, step - 1))}
          disabled={step === 0}
        >
          back
        </button>
        <button type="button" className="btn btn--quiet" onClick={finish}>
          skip the rest — use the baseline
        </button>
        <span className="toolbar__spacer" />
        {last ? (
          <button type="button" className="btn btn--primary" onClick={finish}>
            save and start
          </button>
        ) : (
          <button type="button" className="btn btn--primary" onClick={() => setStep(step + 1)}>
            next
          </button>
        )}
      </div>
    </div>
  );
}

function Choice({
  value,
  onChange,
  options,
}: {
  value: string;
  onChange: (v: string) => void;
  options: [string, string][];
}) {
  return (
    <div className="choice">
      {options.map(([v, label]) => (
        <button
          key={v}
          type="button"
          className={`choice__item${value === v ? " is-on" : ""}`}
          onClick={() => onChange(v)}
        >
          {label}
        </button>
      ))}
    </div>
  );
}
