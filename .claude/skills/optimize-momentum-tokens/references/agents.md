# Fan-out: per-token analyst agents

## When (and when not)

- **Compute never fans out to agents.** `run_sweeps.py` is one deterministic job queue (`-j 3`,
  `RAYON_NUM_THREADS=3`, a machine-wide `flock` slot per sim call). The sim is CPU-bound and
  already parallel inside; more processes on 10 cores / 16 GB only contend.
- **Judgment fans out.** Under isolation each token's verdict is independent, needs reading
  (flags, trade concentration, drawdown trade-offs), and eleven of them inline would flood the
  main context. With **≥ 3 tokens**, dispatch one analyst per token **as soon as its
  `<run>/<SYM>.done` marker exists** — analysts work while later tokens are still computing.
  With 1–2 tokens, write the verdicts inline.
- Use the `general-purpose` agent type, in the background, one message with all ready tokens.
- The combination step (candidate file, book A/B, REPORT.md) stays with the main agent.

## Analyst prompt (fill `<SYM>` and `<run>`)

```
You are the per-token analyst for <SYM> in a momentum per-token sweep. Read-only, except for ONE
output file.

Read:
- <run>/<SYM>_per_trail.md — TRUST block, deployed profile, trail overview, one table per trail
- <run>/<SYM>_candidates.json — the same rows as data (knobs, axes, flags, window stats, params)
- .claude/skills/optimize-momentum-tokens/references/reading-rules.md §3–§6

Write exactly one file, <run>/verdicts/<SYM>.json:
{"token": "<SYM>",
 "verdict": "keep" | "change" | "paper-test" | "insufficient",
 "pick": {"trail": <n>, "min": <n>, "lb": <n>, "z": <n, 0 = off>, "regime": "gated"|"exempt", "fb": <n>} | null,
 "per_trail": {"<trail>": "<best row at this rung, ≤ 20 words, or 'none robust'>"},
 "rationale": "<2–4 sentences: which gates the pick clears, what it trades away>",
 "risks": ["<short>", ...]}

Rules:
- Apply the pick rule of reading-rules.md §5 in order. No row clears gates 1–4 ⇒ "keep", pick null.
- Copy the pick from a row's "knobs" in <SYM>_candidates.json — never invent a combination.
- A FAIL in TRUST (tables suppressed) or T3 FAIL-soft ⇒ "insufficient".
- The operator prefers less P&L for less drawdown: read worst / trueDD / worse-tail before Δ.
- Give every trail rung its line in per_trail — the operator chooses the trail.
- Do not edit assets/momentum_tokens.json, do not git commit or push, do not call momentum-sim
  directly (an extra replay goes through common.run_sim in the skill's scripts, which holds the
  slot lock and the history cap).
Return three lines: verdict · pick (or none) · the one number that decided it.
```

## After the analysts

1. Check every `verdicts/<SYM>.json` parses and its pick is a real row (`apply_params.py`
   refuses unknown tokens; `delta_sum_for` refuses a pick that is not a grid cell). A missing or
   invalid verdict ⇒ write it inline, or leave the token "pending" in the report — never guess.
2. `apply_params.py --run-dir <run> --from-verdicts` → `candidate_tokens.json`.
3. `book_ab.py --run-dir <run>` → additivity; split into arms if the gap is material.
4. `build_report.py --run-dir <run>` → present REPORT.md.

Subagents in this repo have pushed to origin/main on their own before — the "no commit, no push"
line in the prompt is not decoration.
