# Run log

One line per run: `date · book · tokens · verdicts · applied? · run dir`. The run's full report
is `assets/per_token_sweep_<date>/REPORT.md` (gitignored). Newest last.

## Before this skill (per-token history carried over from optimize-momentum-config)

- **2026-09-06** — JitoSOL (clean 80 d, N=1, $1000): incumbent `min 3.4 / trail 10 / lb 720`
  +250/+191 → consensus `min 2.55 lb 480` +261/+297; 0.8 cut and delete-the-event residuals held
  → APPLIED (backup `assets/momentum_tokens.json.bak.jito-pre-2.55-lb480`). HYPE/ZEC winners
  (lower bars, trail 30) NOT applied. The regime axis was inert by bug in every sweep before
  2026-09-13, so any "regime never binds" before that date is an artifact.
- **2026-09-12** — green fade bar `fb=0.75` passed the pre-registered rule for HYPE/ZEC (N=1 +8%
  train / +6% test, worst −120 → −112) → APPLIED as absolute bars; nothing for JitoSOL (inert
  until negative).
- **2026-09-14** — trail ladders (ZEC, HYPE, JitoSOL; other knobs pinned): inert above a
  threshold, and below it the worst trade grows 90–130× for ±$30 → keep 30 at the time.
- **2026-09-15** — trail-5 set chosen on the 0.7/0.8 cuts, then REVERSED by five disjoint windows
  (live 5/5 +1077 vs retune 3/5 +727) — origin of rule 2b. Later that day, by best worst window:
  JitoSOL → trail 2 (operator's call, "flattest profile"), HYPE → trail 2.
- **2026-09-19 → 27** — MET/RAY/Jupiter/STONK/CATE/BP/wNEAR/KMNO added or re-tuned; mostly
  `--max-n 1` on multi-token files, 5 bps, the 0.7 cut only, and some 30-day files — the drift this
  skill's scripts now prevent. 09-27: ZEC → 4.3875 / trail 5 / lb 480 / z off / fb 0.5.

## Runs of this skill
- **2026-09-27** · `price_history.book_20260927.jsonl` (158.8 d, 1 row/min) · 10 swept (STONK INSUFFICIENT 34.7 d; CATE SHORT K=3) · change CATE/MET/Jupiter · paper-test HYPE/JitoSOL (book-downgraded), ZEC/KMNO · keep BP (letter: paper-test)/RAY/wNEAR · edge:min extended for ZEC (spike) + JitoSOL (plateau) · book A/B gap +256.82 → 12 arms; rec set Σ back +309.73 · found `replay_multi` z-gate `break` sim/live divergence (penalises HYPE's pick in every multi-token replay) · APPLIED CATE (backup `momentum_tokens.pre_cate_2026-09-27_20260927_195942.bak`) + MET (backup `…pre_met_…_201351.bak`) + HYPE and KMNO on operator request despite paper-test (backups `…pre_hype_…_200235.bak`, `…pre_kmno_…_201113.bak`) · `assets/per_token_sweep_2026-09-27/`
