#!/usr/bin/env python3
"""
measure_costs.py — per-token execution cost for the sim, measured on the route the live
trader actually executes: a Jupiter round-trip quote at the token's live notional.

Why this and not a single .env value: MOMENTUM_SLIPPAGE_BPS is one number for every token,
but an LST fills at ~0.1 bps/leg and a thin meme at tens of bps. Every grid job is an isolated
one-token replay, so each can run at its own cost (env prefix; dotenvy never overrides it).

Why a round-trip Jupiter quote (and not Birdeye, or the trader's own fills):
- it prices the exact path the trader uses (`src/portfolio/jupiter.rs` /quote, same params);
  the output amount is net of LP fees and includes impact on both legs;
- Birdeye exposes prices/liquidity, not an executable quote, and its free tier rate-limits;
- the trader's audit fills (`momentum_actions.jsonl`) mix slippage with mark staleness
  (one JitoSOL entry shows a −11 bps "cost").

The sim's knob is a u32 applied PER LEG (sim.rs entry_fill_price / exit_fill_price); gas is
charged separately (est_gas_bps), so the quote-derived number must not include gas — it doesn't.

Usage:
  python3 measure_costs.py --out <run_dir>/costs.json [--tokens HYPE,ZEC] [--samples 5]
                           [--cost HYPE=2 --cost CATE=40]
"""
import argparse
import json
import math
import statistics
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common  # noqa: E402

USDC_DECIMALS = 6


def choose_sim_cost_bps(samples_bps: list) -> int:
    """Turn measured per-leg round-trip costs (bps, floats, ≥1 sample) into the integer
    MOMENTUM_SLIPPAGE_BPS the sim will charge on EVERY entry and exit of this token.

    Policy (operator decision 2026-09-27, the default): the MEDIAN of the samples — one odd quote
    must not move it — rounded UP to a whole bps (the sim's knob is a u32; rounding up keeps the
    cost conservative, e.g. HYPE's 0.18 → 1, ZEC's 2.57 → 3), never below 1 bps.
    """
    med = statistics.median(samples_bps)
    return max(1, math.ceil(med - 1e-9))  # the epsilon keeps an exact 2.0 at 2 despite float noise


def jupiter_quote(base: str, input_mint: str, output_mint: str, amount_raw: int,
                  retries: int = 4) -> dict:
    """GET {base}/quote with the live trader's parameters (jupiter.rs:88-95)."""
    q = urllib.parse.urlencode({
        "inputMint": input_mint,
        "outputMint": output_mint,
        "amount": str(amount_raw),
        "slippageBps": "50",
        "onlyDirectRoutes": "false",
        "asLegacyTransaction": "false",
    })
    url = f"{base.rstrip('/')}/quote?{q}"
    for attempt in range(retries + 1):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers={"accept": "application/json"}),
                                        timeout=15) as r:
                return json.loads(r.read().decode())
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503, 504) and attempt < retries:
                time.sleep(2 ** (attempt + 1))
                continue
            raise
        except (urllib.error.URLError, TimeoutError):
            if attempt < retries:
                time.sleep(2 ** (attempt + 1))
                continue
            raise
    raise RuntimeError("unreachable")


def round_trip_bps(base: str, mint: str, usdc_notional: float) -> tuple:
    """One sample: USDC→token→USDC at the live notional. Returns (per-leg bps, route labels)."""
    usdc_in = int(round(usdc_notional * 10 ** USDC_DECIMALS))
    buy = jupiter_quote(base, common.USDC_MINT, mint, usdc_in)
    tokens_out = int(buy["outAmount"])
    if tokens_out <= 0:
        raise RuntimeError("buy quote returned 0 tokens")
    sell = jupiter_quote(base, mint, common.USDC_MINT, tokens_out)
    usdc_back = int(sell["outAmount"])
    per_leg = (1.0 - usdc_back / usdc_in) / 2.0 * 10_000.0
    labels = sorted({hop.get("swapInfo", {}).get("label", "?")
                     for leg in (buy, sell) for hop in leg.get("routePlan", [])})
    return per_leg, labels


def parse_overrides(items) -> dict:
    out = {}
    for it in items or []:
        sym, _, val = it.partition("=")
        if not val:
            sys.exit(f"--cost expects SYM=N, got {it!r}")
        out[sym.strip().lower()] = int(val)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", required=True, help="costs.json path (normally <run_dir>/costs.json)")
    ap.add_argument("--tokens-file", default="assets/momentum_tokens.json")
    ap.add_argument("--tokens", default=None, help="comma list of symbols/mints (default: all deployed)")
    ap.add_argument("--samples", type=int, default=5)
    ap.add_argument("--gap-secs", type=float, default=3.0, help="pause between samples")
    ap.add_argument("--cost", action="append", help="SYM=N override (skips the quote for that token)")
    args = ap.parse_args()

    root = common.repo_root()
    env = common.read_env(root)
    base = env.get("MOMENTUM_JUPITER_API_URL", "https://lite-api.jup.ag/swap/v1")
    default_notional = float(env.get("MOMENTUM_TRADE_USDC", 100))
    env_bps = int(float(env.get("MOMENTUM_SLIPPAGE_BPS", 50)))  # trader default
    overrides = parse_overrides(args.cost)
    only = [s.strip() for s in args.tokens.split(",")] if args.tokens else None
    targets = common.deployed_targets(common.load_tokens(root / args.tokens_file), only)

    result = {"generated_at": common.utc(time.time()), "jupiter_base": base,
              "env_slippage_bps": env_bps, "tokens": {}}
    for e in targets:
        sym, mint = e["symbol"], e["mint"]
        notional = float((e.get("params") or {}).get("trade_usdc", default_notional))
        rec = {"mint": mint, "trade_usdc": notional}
        if sym.lower() in overrides:
            rec.update(source="override", used_bps=overrides[sym.lower()])
        else:
            samples, routes, errors = [], set(), []
            for i in range(args.samples):
                if i:
                    time.sleep(args.gap_secs)
                try:
                    bps, labels = round_trip_bps(base, mint, notional)
                    samples.append(round(bps, 4))
                    routes.update(labels)
                except Exception as ex:  # a failed sample is data, not a crash
                    errors.append(str(ex)[:200])
            if samples:
                rec.update(source="quote", samples_bps=samples, median_bps=statistics.median(samples),
                           used_bps=int(choose_sim_cost_bps(samples)), routes=sorted(routes))
            else:
                rec.update(source="fallback", used_bps=env_bps, note="every quote failed — .env value used")
            if errors:
                rec["errors"] = errors
        result["tokens"][sym] = rec
        shown = f"median {rec['median_bps']:.2f}" if "median_bps" in rec else rec["source"]
        print(f"  {sym:<8} ${notional:>6.0f}  per-leg {shown:<16} → sim {rec['used_bps']} bps"
              f"{'  routes ' + ','.join(rec.get('routes', [])) if rec.get('routes') else ''}")
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)  # runs before run_sweeps creates the run dir
    common.write_json(Path(args.out), result)
    print(f"wrote {args.out}")


if __name__ == "__main__":
    main()
