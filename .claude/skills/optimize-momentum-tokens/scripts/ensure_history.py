#!/usr/bin/env python3
"""
ensure_history.py — guarantee a ≥150-day combined history book for every deployed token,
and report whether it can be trusted (the T0 data gate).

Modes:
  --report            (default) coverage/T0 report for the newest book (or --book FILE)
  --build             fetch missing/stale per-series raw files, then merge a NEW dated book

Why it is built this way (every line was a real failure):
- one GeckoTerminal series per `backfill_history.js` invocation, sequentially: the script
  keeps everything in memory until the end, a failed page silently keeps partial data, and
  parallel runs trigger 429 storms. Per-series raw files make the fetch resumable.
- `--no-splice` and an explicit `--output`: the default output OVERWRITES
  assets/price_history.extended.jsonl, and splicing drops GT data inside the live window.
- pools are pinned from the tokens file (SOL to Raydium v4 58oQChx4…): volume-ranked
  auto-pick once chose a 5-week-old pool and produced a 150-d file with no head.
- the plain "SOL" key is stripped (WSOL mint kept): `sanitize_pegged` only runs on files that
  carry "SOL" and is mis-calibrated at 1-min cadence (deletes real HYPE/ZEC moves); the sim
  re-adds the alias AFTER that pass, exactly as for the validated research files.
- the book is a NEW dated file, chmod 444, with a gz copy in assets/history_backups/ (chmod
  444 alone does not protect it — the sim's loader renames over its input).
- statistics are computed on `momentum-sim sanitize-dump` output: raw-JSONL analysis has
  manufactured false signals before (sanitizer-bypass memory).
"""
import argparse
import gzip
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common  # noqa: E402

DAY = 86_400
GLITCH_JUMP, GLITCH_REVERT, GLITCH_OBS = 0.04, 0.015, 15
PEGGED_MEDIAN_RET = 0.001  # median |1-obs return| below this = LST-class (the glitch rule is safe there)


# ── raw per-series fetch ─────────────────────────────────────────────────────────────────

def series_specs(targets: list) -> list:
    """(key, backfill --tokens spec) per series. SOL first (every regime gate needs it)."""
    specs = [("SOL", f"{common.WSOL}:SOL:{common.SOL_POOL}")]
    for e in targets:
        pool = e.get("pool") or ""
        specs.append((e["symbol"], f"{e['mint']}::{pool}"))
    return specs


def raw_path(raw_dir: Path, key: str) -> Path:
    return raw_dir / f"{key}.jsonl"


def series_span(path: Path, key_mint: str) -> tuple:
    first = last = None
    n = 0
    for row in common.iter_jsonl(path):
        if key_mint in row["prices"]:
            n += 1
            first = row["ts"] if first is None else first
            last = row["ts"]
    return first, last, n


def fetch_series(root: Path, spec: str, days: int, out: Path, log: Path) -> bool:
    """One backfill_history.js call. A page that fails after all retries makes the script keep the
    partial series and still exit 0 — so the log is scanned and a `<key>.partial` marker written;
    a later --build re-fetches a marked series instead of trusting it."""
    before = log.read_text().count("failed after retries") if log.exists() else 0
    tmp = out.with_suffix(".jsonl.tmp")
    cmd = ["node", "scripts/backfill_history.js", "--days", str(days), "--no-splice",
           "--output", str(tmp), "--tokens", spec]
    with open(log, "a") as lf:
        lf.write(f"\n# {common.utc(time.time())} {' '.join(cmd)}\n")
        lf.flush()
        rc = common.run_child(cmd, cwd=root, stdout=lf, stderr=subprocess.STDOUT)  # tracked for the cascade
    if rc == 0 and tmp.exists() and tmp.stat().st_size > 0:
        tmp.replace(out)
        marker = out.with_suffix(".partial")
        if log.read_text().count("failed after retries") > before:
            marker.write_text("a GeckoTerminal page failed after retries; the series may be truncated\n")
            print(f"  WARNING {out.name}: a page failed after retries — kept as PARTIAL (re-fetched next --build)",
                  file=sys.stderr)
        else:
            marker.unlink(missing_ok=True)
        return True
    return False


def newest_raw(root: Path, key: str, exclude: Path):
    """Newest earlier raw file for this series (assets/history_raw/<date>/<key>.jsonl)."""
    cands = sorted(p for p in (root / "assets" / "history_raw").glob(f"*/{key}.jsonl") if p.parent != exclude)
    return cands[-1] if cands else None


def top_up(root: Path, key: str, spec: str, prev: Path, out: Path, days: int, log: Path) -> bool:
    """Reuse an earlier raw series: fetch only the missing tail and union it in. SOL alone is
    ~40 min for 150 d; a daily re-run should cost minutes, not the full fetch."""
    mint = spec.split(":")[0]
    first, last, _ = series_span(prev, mint)
    if last is None:
        return False
    gap_days = int((time.time() - last) // DAY) + 1
    if first is not None and first > time.time() - days * DAY + 2 * DAY:
        return False  # the earlier file never reached back far enough: refetch in full
    if gap_days <= 1:
        shutil.copyfile(prev, out)
        return True
    tail = out.with_suffix(".tail.jsonl")
    if not fetch_series(root, spec, gap_days, tail, log):
        return False
    # the fresh tail wins on overlapping minutes: the earlier file's last candle may have been
    # captured while that minute was still in progress
    rows = union_rows([tail, prev], strip_sol=False)
    cutoff = time.time() - (days + 1) * DAY
    part = out.with_suffix(".jsonl.part")
    with open(part, "w") as f:
        for r in rows:
            if r["ts"] >= cutoff:
                f.write(common.json.dumps(r, separators=(",", ":")) + "\n")
    part.replace(out)
    tail.unlink(missing_ok=True)
    return True


def union_rows(paths: list, strip_sol: bool = True) -> list:
    """ts-union of several {ts, prices} JSONL files. On a (ts, key) conflict the FIRST file
    wins (GeckoTerminal candles are immutable, so a conflict is a duplicate, not a disagreement).
    The plain "SOL" key is dropped; the WSOL mint key is kept."""
    grid = {}
    for p in paths:
        for row in common.iter_jsonl(p):
            prices = grid.setdefault(int(row["ts"]), {})
            for k, v in row["prices"].items():
                if strip_sol and k == "SOL":
                    continue
                if k not in prices and isinstance(v, (int, float)) and v > 0:
                    prices[k] = v
    return [{"ts": ts, "prices": prices} for ts, prices in sorted(grid.items()) if prices]


def write_book(root: Path, rows: list) -> Path:
    day = time.strftime("%Y%m%d", time.gmtime())
    dst = root / "assets" / f"price_history.book_{day}.jsonl"
    n = 2
    while dst.exists():
        dst = root / "assets" / f"price_history.book_{day}_{n}.jsonl"
        n += 1
    with open(dst, "w") as f:
        for r in rows:
            f.write(common.json.dumps(r, separators=(",", ":")) + "\n")
    os.chmod(dst, 0o444)
    backup_dir = root / "assets" / "history_backups"
    backup_dir.mkdir(exist_ok=True)
    with open(dst, "rb") as src, gzip.open(backup_dir / (dst.name + ".gz"), "wb") as gz:
        shutil.copyfileobj(src, gz)
    return dst


# ── coverage / T0 statistics (on what the sim actually sees) ─────────────────────────────

def sanitized_rows(root: Path, book: Path, build: bool = True) -> list:
    """`momentum-sim sanitize-dump` of the book, via a symlink in a temp dir (the loader can
    only ever rename over the symlink, never the canonical file)."""
    common.ensure_binary(root, build=build)
    with tempfile.TemporaryDirectory() as td:
        link = Path(td) / "book.jsonl"
        link.symlink_to(book.resolve())
        out = Path(td) / "sanitized.jsonl"
        rc = common.run_sim(root, ["sanitize-dump", "--history", link, "--output", out],
                            Path(td) / "sanitize.log", slots=3, guard=common.Tripwire(book))
        if rc != 0:
            sys.exit(f"sanitize-dump failed: {(Path(td) / 'sanitize.log').read_text()[-800:]}")
        return list(common.iter_jsonl(out))


def token_stats(rows: list, mint: str, t0: int, t1: int) -> dict:
    series = [(r["ts"], r["prices"][mint]) for r in rows if mint in r["prices"]]
    if not series:
        return {"prints": 0}
    first, last = series[0][0], series[-1][0]
    days = max((last - first) / DAY, 1e-9)
    rets = [abs(b[1] / a[1] - 1.0) for a, b in zip(series, series[1:]) if a[1] > 0]
    return {
        "prints": len(series), "first": first, "last": last, "days": round(days, 1),
        "prints_per_day": round(len(series) / days, 1),
        "median_abs_ret": statistics.median(rets) if rets else 0.0,
        "glitches": count_glitches([p for _, p in series]),
        "starts_late_days": round((first - t0) / DAY, 1),
    }


def glitch_spans(prices: list) -> list:
    """The JitoSOL cleaning rule (2026-08-29): a print that jumps >4% from the last good price
    and returns to within 1.5% of it within 15 observations. Returns (start, back) index pairs;
    prints start..back-1 are the excursion, `back` is the first good print again."""
    n, spans, i = len(prices), [], 1
    while i < n:
        base = prices[i - 1]
        if base > 0 and abs(prices[i] / base - 1.0) > GLITCH_JUMP:
            back = next((j for j in range(i + 1, min(n, i + 1 + GLITCH_OBS))
                         if abs(prices[j] / base - 1.0) <= GLITCH_REVERT), None)
            if back is not None:
                spans.append((i, back))
                i = back + 1
                continue
        i += 1
    return spans


def count_glitches(prices: list) -> int:
    return len(glitch_spans(prices))


def clean_pegged(rows: list, mints: list) -> dict:
    """--clean-pegged: drop the glitch excursions of LST-class tokens only (median |1-obs return|
    below PEGGED_MEDIAN_RET — an LST cannot move 4% and snap back within 15 minutes; a meme can,
    so memes are never touched). Mutates `rows`; returns {mint: prints removed}."""
    removed = {}
    for mint in mints:
        series = [(i, r["prices"][mint]) for i, r in enumerate(rows) if mint in r["prices"]]
        rets = [abs(b[1] / a[1] - 1.0) for a, b in zip(series, series[1:]) if a[1] > 0]
        if len(series) < 3 or not rets or statistics.median(rets) >= PEGGED_MEDIAN_RET:
            continue
        n = 0
        for start, back in glitch_spans([px for _, px in series]):
            for j in range(start, back):
                del rows[series[j][0]]["prices"][mint]
                n += 1
        removed[mint] = n
    return removed


def cadence_shift(rows: list) -> float:
    """Worst 7-day rows/day relative to the file median (lookback_obs counts rows, not time:
    a cadence change silently changes what a lookback means)."""
    if len(rows) < 2:
        return 0.0
    buckets = {}
    for r in rows:
        buckets[r["ts"] // (7 * DAY)] = buckets.get(r["ts"] // (7 * DAY), 0) + 1
    counts = list(buckets.values())[1:-1] or list(buckets.values())  # drop partial edge weeks
    med = statistics.median(counts)
    return max(max(c / med, med / c) for c in counts) if med > 0 else 0.0


def coverage(root: Path, book: Path, targets: list, days: int, max_age_days: float, build: bool = True) -> dict:
    raw_rows = list(common.iter_jsonl(book))
    clean = sanitized_rows(root, book, build)
    t0, t1 = raw_rows[0]["ts"], raw_rows[-1]["ts"]
    sol = token_stats(clean, common.WSOL, t0, t1)
    report = {
        "book": str(book), "rows": len(raw_rows), "first": t0, "last": t1,
        "span_days": round((t1 - t0) / DAY, 1), "age_days": round((time.time() - t1) / DAY, 1),
        "has_plain_sol_key": any("SOL" in r["prices"] for r in raw_rows),
        "truncation_signature": len(raw_rows) == common.TRUNCATION_SIGNATURE_ROWS,
        "cadence_shift": round(cadence_shift(raw_rows), 2),
        "wsol": sol, "tokens": {},
    }
    for e in targets:
        mint = e["mint"]
        raw_n = sum(1 for r in raw_rows if mint in r["prices"])
        st = token_stats(clean, mint, t0, t1)
        st["raw_prints"] = raw_n
        st["sanitizer_removed_pct"] = round(100.0 * (raw_n - st["prints"]) / raw_n, 2) if raw_n else 0.0
        st["status"], st["t0"] = t0_verdict(st, sol, report, days, max_age_days)
        report["tokens"][e["symbol"]] = st
    return report


def t0_verdict(st: dict, sol: dict, rep: dict, days: int, max_age_days: float) -> tuple:
    """(status, [(level, message)]) — status: OK | SHORT | INSUFFICIENT | FAIL."""
    notes = []
    if rep["truncation_signature"]:
        notes.append(("FAIL", f"rows = {common.TRUNCATION_SIGNATURE_ROWS}: the loader truncation signature"))
    if rep["has_plain_sol_key"]:
        notes.append(("FAIL", 'plain "SOL" key present — would trigger the mis-calibrated sanitize_pegged pass'))
    if st.get("prints", 0) == 0:
        notes.append(("FAIL", "no prints for this token in the book"))
        return "FAIL", notes
    if sol.get("prints", 0) == 0 or sol["first"] > rep["first"] + 3 * DAY or sol["last"] < rep["last"] - 3 * DAY:
        notes.append(("WARN", "WSOL does not span the book — the regime gate is blind where it is missing"))
    if st["glitches"]:
        notes.append(("WARN", f"{st['glitches']} glitch candidate(s) (>4% jump, ≤15-obs revert)"))
    if st["sanitizer_removed_pct"] > 1.0:
        notes.append(("WARN", f"sanitizer removed {st['sanitizer_removed_pct']}% of prints — inspect"))
    if st["prints_per_day"] < 60:
        notes.append(("WARN", f"thin series ({st['prints_per_day']} prints/day): sparse rows vs the live forward-fill"))
    if rep["cadence_shift"] > 2.0:
        notes.append(("WARN", f"cadence shift ×{rep['cadence_shift']} between weeks — lookback_obs ≠ the same time"))
    if rep["age_days"] > max_age_days:
        notes.append(("WARN", f"book ends {rep['age_days']} d ago (> {max_age_days}) — top it up"))
    if st["starts_late_days"] > 2:
        notes.append(("WARN", f"token starts {st['starts_late_days']} d after the book (younger token, young pool, "
                              f"or a partial fetch — check the raw .log)"))
    if any(level == "FAIL" for level, _ in notes):
        return "FAIL", notes
    if st["days"] < 60:
        return "INSUFFICIENT", notes + [("INFO", f"{st['days']} d < 60 d: no tuning, keep deployed")]
    if st["days"] < days - 2:
        return "SHORT", notes + [("INFO", f"{st['days']} d < {days} d: own-span windows, K=3")]
    return "OK", notes


def print_coverage(rep: dict):
    print(f"book {rep['book']}  rows {rep['rows']:,}  {common.utc(rep['first'])} → {common.utc(rep['last'])} "
          f"({rep['span_days']} d, ends {rep['age_days']} d ago)  cadence-shift ×{rep['cadence_shift']}")
    w = rep["wsol"]
    print(f"  WSOL: {w.get('prints', 0):,} prints, {w.get('days', 0)} d")
    print(f"  {'token':<9}{'status':<13}{'days':>7}{'prints/d':>10}{'glitch':>8}{'sanit%':>8}  notes")
    for sym, st in rep["tokens"].items():
        notes = "; ".join(f"{lvl}: {msg}" for lvl, msg in st["t0"] if lvl != "INFO") or "—"
        print(f"  {sym:<9}{st['status']:<13}{st.get('days', 0):>7}{st.get('prints_per_day', 0):>10}"
              f"{st.get('glitches', 0):>8}{st.get('sanitizer_removed_pct', 0):>8}  {notes}")


# ── main ─────────────────────────────────────────────────────────────────────────────────

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--build", action="store_true", help="fetch missing/stale series and write a new book")
    ap.add_argument("--book", default=None, help="book to report on (default: newest price_history.book_*)")
    ap.add_argument("--tokens-file", default="assets/momentum_tokens.json")
    ap.add_argument("--tokens", default=None, help="comma list (default: all deployed)")
    ap.add_argument("--days", type=int, default=150)
    ap.add_argument("--max-age-days", type=float, default=7.0)
    ap.add_argument("--raw-dir", default=None, help="per-series raw dir (default assets/history_raw/<today>)")
    ap.add_argument("--no-fetch", action="store_true", help="--build from existing raw files only")
    ap.add_argument("--json", default=None, help="also write the coverage report as JSON here")
    ap.add_argument("--clean-pegged", action="store_true",
                    help="drop glitch excursions of LST-class tokens (median |1-obs return| < 0.1%%) when building")
    args = ap.parse_args()

    root = common.repo_root()
    only = [s.strip() for s in args.tokens.split(",")] if args.tokens else None
    entries = common.load_tokens(root / args.tokens_file)
    targets = common.deployed_targets(entries, only)

    if args.build:
        common.install_cascade("optimize-momentum-tokens:ensure_history --build")  # SIGTERM stops the fetch too
        # A book always carries EVERY deployed series: --tokens only narrows the report. A filtered
        # build would become the "newest book" and silently shrink every later run (and the A/B).
        build_targets = common.deployed_targets(entries)
        raw_dir = Path(args.raw_dir) if args.raw_dir else root / "assets" / "history_raw" / time.strftime("%Y%m%d", time.gmtime())
        raw_dir.mkdir(parents=True, exist_ok=True)
        paths = []
        for key, spec in series_specs(build_targets):
            out = raw_path(raw_dir, key)
            mint = spec.split(":")[0]
            if out.exists() and out.with_suffix(".partial").exists() and not args.no_fetch:
                print(f"{key}: marked PARTIAL by an earlier fetch — re-fetching", flush=True)
                out.unlink()
            if not out.exists() and not args.no_fetch:
                prev = newest_raw(root, key, raw_dir)
                log = raw_dir / f"{key}.log"
                if prev is not None and top_up(root, key, spec, prev, out, args.days, log):
                    print(f"topped up {key} from {prev.parent.name}", flush=True)
                else:
                    print(f"fetching {key} ({args.days} d) …", flush=True)
                    if not fetch_series(root, spec, args.days, out, log):
                        print(f"  FAILED {key} — see {log}", file=sys.stderr)
                        continue
            if out.exists():
                first, last, n = series_span(out, mint)
                print(f"  raw {key:<8} {n:>8,} prints  {common.utc(first) if first else '—'} → "
                      f"{common.utc(last) if last else '—'}")
                paths.append(out)
        if not paths:
            sys.exit("no raw series — nothing to build")
        rows = union_rows(paths)
        if args.clean_pegged:
            removed = clean_pegged(rows, [e["mint"] for e in build_targets])
            summary = ", ".join(f"{m[:8]}…: {n}" for m, n in removed.items()) or "nothing (no LST-class token)"
            print(f"--clean-pegged removed: {summary}")
        book = write_book(root, rows)
        print(f"wrote {book} (chmod 444; gz copy in assets/history_backups/)")
    else:
        book = Path(args.book) if args.book else common.newest_book(root)
        if book is None:
            sys.exit("no assets/price_history.book_*.jsonl yet — run with --build")

    rep = coverage(root, Path(book), targets, args.days, args.max_age_days)
    print_coverage(rep)
    if args.json:
        common.write_json(Path(args.json), rep)


if __name__ == "__main__":
    main()
