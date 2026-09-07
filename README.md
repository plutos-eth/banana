# quarrel

A local desktop sniper terminal and strategy backtester for pons v2 on Robinhood Chain.

> **Status: phase 0 (skeleton).** Not yet usable. See `PLAN.md` for what is built and what is next.

Two halves sharing one rule engine:

- **Sniper** watches every pons v2 launch as it happens, evaluates it against a transparent boolean
  filter, waits at full draw until the opening tax decays under a ceiling, and buys in arrival
  order. Dry run by default.
- **Strategy Lab** indexes on-chain launch history locally, then runs entry filters against that
  history offline and shows what actually happened to the tokens that passed.

The link between them is the point: the *same* `StrategyConfig` the user tunes in the Lab arms the
live sniper. One type, two consumers, no divergence.

Everything is local. Nothing leaves the machine except JSON-RPC to the endpoint you configure. No
telemetry, no vendor server, no third-party price API, no account.

## What it will not do

It does not detect rugs or honeypots. It does not promise profit. Roughly 1 launch in 78 graduates
(measured 2026-09-07; see `docs/FINDINGS.md`), and the Lab is built to show you the denominator
rather than hide it -- including the tokens that passed your filter and died at 0.1x.

`max_multiple` is labelled "max achievable / peak" and never "profit". Nobody sells a memecoin at
its ATH; the peak lasts seconds and is unidentifiable in the moment. The headline number is always
the fixed-hold multiple, because that is what a rule you could actually execute would have produced.

## Requirements

| | |
|---|---|
| OS | Windows 10/11 (the build of record is `x86_64-pc-windows-msvc`) |
| Rust | 1.85+ |
| Toolchain | Visual Studio Build Tools with the **Windows SDK** and the VC++ workload |
| Runtime | WebView2 (preinstalled on Windows 11) |
| Node | 20+ |

> On Windows, run `cargo` from **PowerShell, not Git Bash**. Git Bash puts GNU coreutils' `link`
> ahead of MSVC's `link.exe` on `PATH`, and the resulting failure does not look like a `PATH`
> problem.

## Build

```powershell
git clone <repo> quarrel
cd quarrel
npm install --prefix ui
cargo build --workspace
cargo test --workspace
```

Invariant checks, both of which run in CI:

```powershell
./scripts/check-trust-boundary.ps1   # the key lives in exactly one crate
./scripts/check-ui-invariants.ps1    # no hard-coded styles; the CSP stays strict
```

## Configuration

Copy `.env.example` to `.env`. Every command except live trading works with it empty -- indexing,
backtesting, the feed, watching and scanning all run with **no private key at all**, and the crate
that reads one is not in the CLI's dependency graph.

## Headless commands

The desktop app is the primary surface. These exist for scripting and long unattended indexing, and
none of them requires a key:

```
quarrel doctor [--probe]                verify chain id, contract addresses, factory parameters
quarrel index [--from --to | --update]  backfill or append; resumable
quarrel verify [--limit N]              check the indexed data against itself
quarrel backtest [--strategy FILE]      run a strategy over the local store
quarrel backtest --print-default        write the baseline strategy to stdout, to edit
```

`backtest` takes the built-in baseline of the specification's section 7.1 when given no file, and
prints the funnel, the sample gate and the holding assumptions exactly as the Strategy Lab shows
them. `--json` gives the same result as a document. Neither command touches the network.

## Safety

Dry run is the default and cannot be changed from the UI: real money moves only behind an explicit
`--live` launch flag, and `--live` is never a button. Read `docs/SAFETY.md` before using it --
particularly the section on the plaintext private key, which is a genuine trade-off and is described
honestly rather than glossed.

## Documentation

| file | contents |
|---|---|
| `docs/ARCHITECTURE.md` | crate boundaries, dependency direction, the trust and egress boundaries |
| `docs/STRATEGY.md` | every rule, every default, where each number came from |
| `docs/SAFETY.md` | what can go wrong, key handling, the plaintext trade-off |
| `docs/FINDINGS.md` | measured chain facts, index cost, and the point-in-time socials verification |
| `PLAN.md` | phase plan, decisions, and everything that turned out differently than specified |

## Licence and independence

MIT. See `LICENSE`.

quarrel is an independent project, not affiliated with or endorsed by pons, Uniswap or Robinhood. No
third-party marks are used; all artwork is original.

The bonding-curve arithmetic is ported from [bodkin](https://github.com/Phosphenq/bodkin) (MIT),
which credits `slightlyuseless/pons-sniper` (MIT). Its operation order is preserved deliberately
rather than tidied: it mirrors the deployed `PonsV2BondingCurve` contract, and the contract is the
authority.
