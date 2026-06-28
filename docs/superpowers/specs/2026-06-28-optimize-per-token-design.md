# optimize-momentum-config: global + per-token — SP4 — Design

**Date:** 2026-06-28
**Status:** Auto-designed under delegated autonomy (user-directed: "the skill
optimize-momentum-config should optimize both global and per-token optimal params").
**Scope:** The `optimize-momentum-config` skill (`scripts/optimize_momentum.py` + `SKILL.md`)
only. Reuses the SP2 Rust `per-token-tune` subcommand; no Rust changes.

## Problem & goal

`optimize-momentum-config` currently grid-searches the **global** config and writes the 6
managed knobs to `.env`. The user wants one invocation to optimize **both**: global
(→`.env`, existing) **and** per-token `{min_metric, trail_pct, max_run_pct}`
(→`momentum_tokens.json`). SP2 already built the Rust `per-token-tune` subcommand that
computes each token's best params, runs the 3-arm validation, and (with `--apply`) writes
`momentum_tokens.json`. SP4 wires it into the skill.

## Decisions (autonomous)

1. **Reuse `per-token-tune`** — do not reimplement per-token tuning in Python. The skill
   invokes `momentum-sim per-token-tune` and relays its output.
2. **Per-token runs by default** (the user wants both); a `--no-per-token` flag skips it
   (restores the old global-only behavior).
3. **`--apply` writes both:** the global section writes `.env` (existing, backs up to
   `.env.bak`); the per-token step is invoked with `--apply` so it writes
   `momentum_tokens.json` (the Rust subcommand dedups + preserves entries).
4. **Pass-through:** `--tokens` and `--min-trades` are forwarded to `per-token-tune`. Pool
   defaults inside the subcommand to `.env`'s `MOMENTUM_TRADE_USDC` (no need to pass).
5. **Accepted redundancy:** `per-token-tune` internally re-grids global (for its 3-arm
   validation Arms A/B), so the global grid runs twice (once in the skill's global section,
   once inside per-token-tune). The grid is fast (seconds); the duplication keeps both tools
   self-contained, and the per-token step's 3-arm verdict is useful extra output. Documented.

## Behavior

`python3 optimize_momentum.py [--apply] [--no-per-token] [--tokens …] [--min-trades N]`:
1. **Global section (unchanged):** grid → head-to-head → proposed `.env` changes → (with
   `--apply`) write `.env`.
2. **Per-token section (new, unless `--no-per-token`):** print a header, then run
   `momentum-sim per-token-tune --min-trades N --tokens <tokens> [--apply]`, streaming its
   stdout (per-token table + 3-arm validation + verdict). With `--apply` it writes
   `momentum_tokens.json`; without, it's preview-only and prints the re-run hint.
3. Final caveat (unchanged), extended to note per-token params are a hypothesis to
   paper-validate like the global config.

## Files

- `scripts/optimize_momentum.py`: add `--no-per-token` arg; after the global section
  (before the final caveat), a `run_per_token(binp, root, tokens, min_trades, apply)` step
  that subprocess-invokes `per-token-tune` and relays output. ~25 lines.
- `SKILL.md`: document that the skill now optimizes global (→.env) **and** per-token
  (→momentum_tokens.json); the `--no-per-token` escape hatch; that per-token feeds the
  multi-slot trader (SP3); paper-first caveat.

## Testing

- `python3 optimize_momentum.py --help` lists `--no-per-token`.
- Preview run (`python3 optimize_momentum.py`, no `--apply`) prints BOTH the global
  head-to-head AND the per-token section (per-token table + 3-arm verdict), and writes
  nothing (momentum_tokens.json unchanged — verify via diff).
- `--no-per-token` reproduces the old global-only output (no per-token section).
- `--apply` path is verified on a COPY of momentum_tokens.json (or restored after) to
  confirm per-token params are written and re-parse — the underlying writer is already
  tested in SP2.

## Out of scope

- Rust changes (per-token-tune already exists).
- Changing the global grid logic or the 6 managed knobs.

## Success criterion

One `optimize_momentum.py --apply` run writes the best global config to `.env` AND the best
per-token params to `momentum_tokens.json`, printing both the global head-to-head and the
per-token 3-arm verdict; `--no-per-token` restores global-only; nothing is written in
preview mode.
