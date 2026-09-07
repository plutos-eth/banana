/**
 * The detail drawer: every read, every rule evaluation, and where to check them.
 *
 * Spec §8 view 1 asks for exactly that, and §3.4 for refusals that name the rule and the
 * values. So this shows the point-in-time features the evaluator actually saw, each rule
 * beside its verdict, and the post-entry facts in their own section — labelled as the
 * future relative to entry, because they are a different type in the backend for that
 * reason (§5.3).
 *
 * The deployer's declared links are shown as text and opened, if at all, in the user's own
 * browser through a command that refuses anything but the explorer. Nothing here is
 * fetched (PLAN.md C1).
 */

import { useEffect, useState } from "react";
import { api, type LaunchDetail } from "../ipc";
import { bps, clock, multiple, shortHex } from "../format";
import { useApp } from "../store";

export function Drawer({ token, onClose }: { token: string; onClose: () => void }) {
  const [detail, setDetail] = useState<LaunchDetail | null>(null);
  const setError = useApp((s) => s.setError);

  useEffect(() => {
    let live = true;
    api
      .launch(token)
      .then((d) => {
        if (live) setDetail(d);
      })
      .catch((e) => setError(e));
    return () => {
      live = false;
    };
  }, [token, setError]);

  return (
    <aside className="drawer" aria-label="launch detail">
      <div className="drawer__head">
        <span className="drawer__title mono">{detail?.row.symbol ?? shortHex(token)}</span>
        <button type="button" className="btn" onClick={onClose}>
          close <kbd className="kbd">esc</kbd>
        </button>
      </div>

      {!detail ? (
        <div className="drawer__body">reading…</div>
      ) : (
        <div className="drawer__body">
          <Section title="decision">
            <div className={`verdict ${detail.row.passed ? "is-pass" : "is-refuse"}`}>
              {detail.row.passed ? "PASS" : "REFUSED"}
            </div>
            <table className="kv">
              <tbody>
                {detail.rules.map((r) => (
                  <tr key={r.rule}>
                    <th>{r.rule}</th>
                    <td className={r.passed ? "is-pass" : "is-refuse"}>
                      {r.passed ? "ok" : (r.detail ?? "refused")}
                    </td>
                  </tr>
                ))}
                {detail.rules.length === 0 && (
                  <tr>
                    <th>no rules</th>
                    <td>an empty filter passes everything</td>
                  </tr>
                )}
              </tbody>
            </table>
          </Section>

          <Section title="read at launch">
            <p className="note">
              Everything below comes from blocks strictly before this launch, or from its
              own transaction. It is what the filter saw.
            </p>
            <table className="kv">
              <tbody>
                <Kv k="block" v={String(detail.row.block)} />
                <Kv k="time" v={clock(detail.row.ts)} />
                <Kv k="pair" v={detail.row.pair} />
                <Kv k="dev buy" v={bps(detail.features.dev_buy_bps)} />
                <Kv k="creator tax" v={bps(detail.features.creator_tax_bps)} />
                <Kv
                  k="exempt wallets"
                  v={detail.features.exempt_wallets === null ? "unreadable" : String(detail.features.exempt_wallets)}
                />
                <Kv k="fee recipient" v={detail.features.fee_recipient} />
                <Kv k="twitter" v={detail.features.socials.twitter} />
                <Kv k="website" v={detail.features.socials.website} />
                <Kv k="telegram" v={detail.features.socials.telegram} />
                <Kv k="deployer launches" v={String(detail.features.deployer_launches)} />
                <Kv k="deployer graduations" v={String(detail.features.deployer_graduations)} />
                <Kv k="farm twins (30m)" v={String(detail.features.fingerprint_twins_30m)} />
                <Kv
                  k="deployer history depth"
                  v={`${detail.features.deployer_history_depth_blocks} blocks`}
                />
              </tbody>
            </table>
          </Section>

          {detail.outcome && (
            <Section title="what became of it">
              <p className="note">
                The future relative to entry. Display only — no rule can read these, and
                the peak is the best price that existed, not one anybody captured.
              </p>
              <table className="kv">
                <tbody>
                  <Kv k="entry price" v={detail.outcome.has_entry ? detail.outcome.entry_rule : "none established"} />
                  <Kv k="held 5 minutes" v={multiple(detail.outcome.mult_after_5m_bps)} />
                  <Kv k="held 30 minutes" v={multiple(detail.outcome.mult_after_30m_bps)} />
                  <Kv k="peak (not captured)" v={multiple(detail.outcome.max_multiple_bps)} />
                  <Kv k="migrated" v={detail.outcome.migrated ? "yes" : "no"} />
                  <Kv k="trades after entry" v={String(detail.outcome.post_entry_trades)} />
                </tbody>
              </table>
            </Section>
          )}

          <Section title="check it yourself">
            <div className="links">
              <Link label="token" url={detail.links.token} />
              <Link label="curve" url={detail.links.curve} />
              <Link label="deployer" url={detail.links.deployer} />
              <Link label="launch tx" url={detail.links.tx} />
            </div>
            {detail.links.socials.length > 0 && (
              <>
                <p className="note">
                  Declared by the deployer. Shown as text and never fetched: loading one
                  would tell them who is watching, in real time, before anybody buys.
                </p>
                <table className="kv">
                  <tbody>
                    {detail.links.socials.map(([k, v]) => (
                      <tr key={k}>
                        <th>{k}</th>
                        <td className="mono is-wrap">{v}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </>
            )}
          </Section>
        </div>
      )}
    </aside>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="drawer__section">
      <h3 className="drawer__section-title">{title}</h3>
      {children}
    </section>
  );
}

function Kv({ k, v }: { k: string; v: string }) {
  return (
    <tr>
      <th>{k}</th>
      <td className="mono">{v}</td>
    </tr>
  );
}

function Link({ label, url }: { label: string; url: string }) {
  const setError = useApp((s) => s.setError);
  return (
    <button
      type="button"
      className="btn btn--link"
      title={url}
      onClick={() => {
        api.openExplorer(url).catch((e) => setError(e));
      }}
    >
      {label} ↗
    </button>
  );
}
