---
name: block-whole-file-rustfmt
enabled: true
event: bash
action: block
pattern: (^|[\n;&|(])\s*(cargo(\s+\+\S+)?\s+fmt|rustfmt)\b(?!.*--check)
---

🛑 **Blocked: whole-file `cargo fmt` / `rustfmt` is banned in this repo.**

This codebase is **not** rustfmt-clean. Running `cargo fmt` (whole crate) or
`rustfmt <file>` (whole file) reformats everything and buries your real change
under thousands of formatting-only lines. It has bitten us twice (a 55-file
`cargo fmt` blast, and a 4-file `rustfmt` that turned a ~500-line change into
2486/601).

**Do this instead:**
- Format only the lines you edit, by hand — write new code matching the
  surrounding indentation/style. The Edit tool leaves untouched lines untouched.
- Read-only checks are allowed: `cargo fmt --check` / `cargo fmt -- --check`.

**If a commit is already polluted (and not yet pushed), recover the clean diff:**
1. `git show HEAD~1:<file> > orig` ; `cp orig fmt` ; `rustfmt --edition 2021 fmt`
2. `git merge-file -p orig fmt <current-file> > clean` (pristine formatting +
   only your semantic edits). At any conflict, keep the **current** side.
3. Watch for duplicate `use` lines introduced by the merge; remove them.
4. Verify equivalence with the oracle: `rustfmt(clean)` must equal the committed
   file (0 diff) — proves `clean` is semantically identical, just un-reformatted.
5. Install the clean files, `cargo test --lib`, then `git commit --amend`.

See memory `feedback-no-whole-file-rustfmt` for the full rationale.
