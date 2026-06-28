---
name: vet-momentum-token
description: >-
  Vet a single token for the momentum trader end-to-end: given a ticker, name, or mint,
  backfill its 1-minute price history from Birdeye, run the full momentum-sim grid, and —
  if it's volatile enough AND robustly PnL-positive — add it to the curated watch list
  (assets/momentum_tokens.json). Use this whenever the user wants to check, screen, vet,
  evaluate, backtest, or consider adding a specific token to the momentum watch list —
  e.g. "is WIF worth adding to momentum", "vet JTO for the momentum trader", "backtest
  GRASS and add it if it's good", "should I add POPCAT to the watch list", "check if this
  mint is a good momentum name" — even if they don't say "grid" or "backtest". Specific to
  this repo's momentum-sim + Birdeye backfill + add_momentum_token.js.
---

# Vet Momentum Token

Given a token, fetch the history the backtest needs, grid-search it, and add it to the
curated momentum watch list **only if it clears the bar**: volatile enough to trend, and
robustly PnL-positive.

## Why it works this way

- **Self-contained backfill, no races.** The live `price_history.jsonl` is append-only by a
  running `portfolio-watcher`. Rewriting it or starting a second watcher would corrupt it.
  So the script fetches the candidate's 1-minute candles from Birdeye into a **temp**
  history, and reuses SOL **read-only** from the live file (nearest-minute match). The real
  history is untouched; once the token is curated, the live watcher backfills it for real on
  its next restart.
- **Fixed-trail grid.** The grid runs with `--no-vol-stops` so the winning config uses a
  fixed-% trailing stop — the only stop the live trader can actually execute (it has no
  vol-stop env knob). A paper winner the live trader couldn't reproduce would be useless.
- **Verified-only adds.** Adding goes through `scripts/add_momentum_token.js`, which requires
  an exact Jupiter-**verified** symbol/name match (or an explicit mint) — this is what keeps
  look-alike scam tokens out of the list.

## Qualifying bar (default)

- **Volatile:** spike-filtered annualized vol ≥ **150%** (clearly above SOL's ~130%).
- **PnL-positive:** the grid (fixed-trail, **regime OFF**) finds ≥ **1 robust** config —
  profitable in BOTH train and test slices, ≥ min-trades each — with best **worst-slice**
  P&L > 0.

Both must hold. Tune with `--vol-floor`, `--min-trades`, `--days`.

**What "qualifies" means (and doesn't):** it means the token has a *robust momentum edge
under some fixed-trail config* — the right bar for **list membership**, since the live
ranker (and `optimize-momentum-config`) pick the actual config later. It does **not** mean
the token is profitable under your current live `.env` config. (Example: ORCA qualifies —
it has 36 robust configs — even though one specific config we tried lost badly.) Vetting
runs **regime off** on purpose: SOL is too sparse in the temp history to gate reliably, so
regime-off is the honest baseline; if a token clears the bar without regime help, that's a
real signal.

## Steps

1. **Vet (no list change).** Run the bundled script from the repo root with the user's
   ticker/name/mint:

   ```bash
   node .claude/skills/vet-momentum-token/scripts/vet_token.js <TICKER|NAME|MINT>
   ```

   It resolves the token (verified), backfills ~30d of 1m history from Birdeye (needs
   `BIRDEYE_API_KEY` in `.env`), builds the temp history, runs the grid, and prints a
   VERDICT: the annualized vol vs floor, the robust-config count, best worst-slice/test
   P&L, win%, maxDD, and **QUALIFIES** or **REJECTED**. Nothing is added yet.

   Useful flags: `--days N` (history window, default 30), `--vol-floor PCT` (default 150),
   `--min-trades N` (robustness gate, default 3), `--force` (accept the top verified match
   when there's no exact symbol hit, or an explicit unverified mint).

2. **Relay the verdict and decide.** Show the user the volatility and grid numbers. If
   REJECTED, say why (not volatile enough, or no robust profitable config) and stop — don't
   add a token that didn't clear the bar. If the script reports **insufficient history**
   (Birdeye doesn't cover it at 1m), report that; it can't be vetted.

3. **Add only on confirmation.** If it QUALIFIES and the user wants it added, re-run with
   `--add`:

   ```bash
   node .claude/skills/vet-momentum-token/scripts/vet_token.js <TICKER|NAME|MINT> --add
   ```

   This appends it via `add_momentum_token.js` (dedups by mint; verified-gated). Then tell
   the user to **restart the portfolio-watcher** so it backfills the token into the live
   history and starts ranking it.

## Guardrails

- **Don't `--add` without confirmation** unless the user explicitly asked for a one-shot
  "vet and add if good." Default is vet → show → confirm → add.
- **Respect the qualifying bar.** If the user pushes to add a REJECTED token anyway, you can
  run `add_momentum_token.js <mint>` directly, but flag that it failed vetting (low vol or no
  robust edge) so the choice is informed.
- **State the caveat honestly.** This is a backtest on Birdeye 1m over a finite window —
  small trade counts and understated drawdown are normal. It's a hypothesis to validate in
  paper mode, not a proven edge. Newly-listed tokens with little history are especially
  uncertain.
- **`assets/momentum_tokens.json` is the hand-curated list** — appending to it is expected
  and safe (it's how `add_momentum_token.js` works). The script only ever appends a deduped,
  verified entry.
