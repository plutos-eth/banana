# Security

quarrel signs transactions with a private key you give it. This page says exactly how that key is
handled, what the program sends over the network, and what it deliberately refuses to do. Read it
before LIVE mode, not after.

## The private key is stored in plaintext

You paste it into Settings. It is written to `wallet.key` in the data directory (see the README for
where that is on your platform), unencrypted, by exactly one crate — `quarrel-live`, the only crate
in the workspace permitted to see it.

**This is a real trade-off and not the safest option available.** A plaintext key on disk is
readable by any process running as you, by any backup that captures your home directory, and by
anyone who gets the machine. It is chosen because the alternative — a passphrase prompt on every
start — fights a tool whose entire purpose is to react in under three seconds.

What limits the damage:

- **The key is needed only for LIVE.** Indexing, backtesting, the feed and TEST mode run with no key
  present at all. A TEST session does not check a flag before signing; it holds no key and has
  nothing to sign with. Every other check in the program could be deleted and it still could not
  spend.
- **The session budget caps total exposure** regardless of signal quality, along with a per-buy cap,
  a per-position cap and a maximum number of open positions. These are types, not `if` statements:
  the executor cannot be called without a permission object that only the guards can create.
- **Fund the wallet with what you are prepared to lose.** quarrel is not a custody tool. Use a fresh
  wallet, not your main one.

The key never comes back out. Saving one returns the **address** it derives, and that address is all
the user interface ever sees — a screenshot or a screen share leaks nothing that can spend. It
cannot be changed or removed while a LIVE session is running.

`PRIVATE_KEY` in the environment still works as a fallback for scripts. The file wins when both are
present.

## What quarrel sends over the network

JSON-RPC to the endpoints in `RPC_URL`, and nothing else. No telemetry, no vendor server, no price
API, no crash reporting, no update check, no analytics.

The frontend has no network access of its own. The webview's Content-Security-Policy allows
`connect-src 'self' ipc:` and no external origin at all, so every chain request goes through the
Rust backend. `scripts/check-ui-invariants.ps1` compares each CSP directive **exactly** — not by
substring — so widening one has to be a deliberate edit to that script, reviewed as such.

### quarrel never fetches a token's logo

A launch's calldata contains a logo URL chosen by the token's deployer. Fetching it would tell them
the IP address of everyone watching their launch, in real time, before those people buy — a sniper
announcing itself to the person it is sniping.

Logos are not fetched. The URL is shown as text so it can be inspected. Explorer links are anchors
handed to your system browser, which is your action rather than the program's.

## The trust boundary

`quarrel-live` is the only crate that may read a private key or produce a signature. Nothing below
it may depend on it — not `core`, `chain`, `store`, `indexer`, `backtest`, or the CLI. The literal
string `PRIVATE_KEY` appears in exactly one crate.

This is enforced by `scripts/check-trust-boundary.ps1` in CI, not by review. Adding a dependency
edge into that crate fails the build.

## The IPC surface

The desktop frontend can call a fixed list of named commands and nothing else. The list is asserted
to stay small by `crates/app/tests/ipc_surface.rs`, because every entry is something a compromised
frontend could cause. Tauri core permissions are granted individually in
`crates/app/capabilities/default.json`: two event verbs so the UI can receive progress, and four
window verbs because the application draws its own titlebar. Not filesystem, not shell, not HTTP.

## Releases are not code-signed

Builds come from GitHub Actions on a tagged commit and are unsigned, which is why your operating
system warns about them. Every release publishes `SHA256SUMS.txt`; verify the file you downloaded
before running something that will hold a key. The workflow that produced it is in the repository
and readable.

## Reporting a vulnerability

Open a [security advisory](../../security/advisories/new) rather than a public issue, and please
include what an attacker would gain. If it involves loss of funds, say so first.

There is no bug bounty. This is a personal project published in the hope it is useful.

## What this program does not protect you from

- **Rugs, honeypots and scams.** It reads what a launch declared and what its deployer did before.
  It cannot tell you whether the contract will let you sell.
- **Losing money on a good decision.** Roughly 1 launch in 78 graduates.
- **Your own configuration.** The guards cap what a session can spend. They do not know whether your
  rules are sensible.
