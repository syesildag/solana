# gRPC Pricing for Scanner Discoveries — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Scanner-discovered momentum tokens whose main pool is on PumpSwap get live gRPC vault pricing within one scan tick, without touching `pools.json`.

**Architecture:** Three stages: (1) `scan_tokens.js` enriches its `--json` output with the best PumpSwap pool per survivor (DexScreener, highest-24h-volume rule); (2) the watcher decodes those pools ad-hoc by spawning the existing `fetch_pumpswap_pools.js --pools … --output …` (vault↔mint cross-check included); (3) the gRPC feed bootstrap moves from the bin into the lib and gains an `extra_pools` parameter so the watcher can re-spawn the feed with curated ∪ discovered pools when the dynamic pool set changes, swapping the feed handle and aborting the old stream task.

**Tech Stack:** Node (scan/decoder scripts, `node --test`), Rust/tokio (watcher), Yellowstone gRPC (existing stream client), serde.

**Spec:** `docs/superpowers/specs/2026-07-22-grpc-priced-scan-discoveries-design.md`

## Global Constraints

- `pools.json` is NEVER written by this feature (generated file; dynamic pools live in memory only).
- NEVER run `cargo fmt` / `rustfmt` on whole files (repo is not rustfmt-clean — huge diff churn).
- Curated wiring is authoritative: on pool-id collision, the `pools.json` entry wins over an ad-hoc decoded one.
- All failure paths keep the previous feed / fall back to REST — a scan can degrade pricing freshness, never break it.
- Rust tests: `cargo test --lib <filter>`. JS tests: `node --test scripts/scan_tokens.test.js`.
- Commit after each task; commit only (do NOT push).

---

### Task 1: Scanner pool enrichment (`pickPumpswapPool` + `annotatePools`)

**Files:**
- Modify: `scripts/scan_tokens.js` (OPTS block ~line 40; after `rankSurvivors` call in `main()` ~line 300; exports ~line 340; header docs ~line 20)
- Test: `scripts/scan_tokens.test.js` (append)

**Interfaces:**
- Consumes: DexScreener `GET /latest/dex/tokens/<mint>` → `{ pairs: [{dexId, pairAddress, volume:{h24}, quoteToken:{symbol}}] }`; existing `MINT_RE`, `sleep`, `numEnv`.
- Produces: `--json` rows may carry `pool: "<pumpswap pool address>"` and `quote: "SOL"|"USDC"` (Task 2 consumes these exact field names). Exports `pickPumpswapPool(pairs)`.

- [ ] **Step 1: Write the failing tests** — append to `scripts/scan_tokens.test.js`:

```js
// ── pickPumpswapPool: dynamic-wiring pool picker ────────────────────────────────
const { pickPumpswapPool } = require("./scan_tokens");

const pair = (dexId, pairAddress, volH24, quoteSym) =>
  ({ dexId, pairAddress, volume: { h24: volH24 }, quoteToken: { symbol: quoteSym } });

test("pickPumpswapPool returns the highest-24h-volume pumpswap pool", () => {
  const pairs = [
    pair("pumpswap", RAY, 100_000, "SOL"),
    pair("pumpswap", BONK, 900_000, "SOL"),   // higher volume — must win
    pair("meteora", WIF, 50_000, "SOL"),
  ];
  assert.deepEqual(pickPumpswapPool(pairs), { pool: BONK, quote: "SOL" });
});

test("pickPumpswapPool returns null when the BEST pool is not pumpswap", () => {
  // a lesser pumpswap pool exists, but pricing must follow the dominant venue
  const pairs = [
    pair("raydium", RAY, 900_000, "SOL"),
    pair("pumpswap", BONK, 100_000, "SOL"),
  ];
  assert.equal(pickPumpswapPool(pairs), null);
});

test("pickPumpswapPool normalizes quote and rejects exotic quotes", () => {
  assert.deepEqual(pickPumpswapPool([pair("pumpswap", RAY, 1, "USDC")]), { pool: RAY, quote: "USDC" });
  assert.equal(pickPumpswapPool([pair("pumpswap", RAY, 1, "ORE")]), null);
});

test("pickPumpswapPool handles empty/malformed input", () => {
  assert.equal(pickPumpswapPool([]), null);
  assert.equal(pickPumpswapPool(undefined), null);
  assert.equal(pickPumpswapPool([{ dexId: "pumpswap" }]), null); // no pairAddress
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `node --test scripts/scan_tokens.test.js`
Expected: FAIL — `pickPumpswapPool is not a function`

- [ ] **Step 3: Implement** — in `scripts/scan_tokens.js`:

Add to `OPTS` (after `changeWindow`):

```js
  // How many top survivors get a DexScreener best-pool lookup so the watcher can
  // gRPC-wire them dynamically (spec 2026-07-22). Only pumpswap SOL/USDC pools are
  // wireable; others stay REST. 0 disables enrichment entirely.
  poolEnrichMax: numEnv("SCAN_POOL_ENRICH_MAX", 5),
```

Add above `main()`:

```js
/**
 * PURE pool picker for scan discoveries: from a DexScreener `pairs` array, return
 * {pool, quote} for the HIGHEST-24h-VOLUME pair — but only when that pair is on
 * pumpswap with a SOL/USDC quote (the only venue the watcher can decode+wire
 * dynamically). Volume, never liquidity, picks the pool (fake-TVL rule).
 * Null = token stays REST-priced.
 */
function pickPumpswapPool(pairs) {
  if (!Array.isArray(pairs) || pairs.length === 0) return null;
  const best = [...pairs].sort(
    (a, b) => ((b.volume && +b.volume.h24) || 0) - ((a.volume && +a.volume.h24) || 0)
  )[0];
  if (!best || best.dexId !== "pumpswap") return null;
  if (!MINT_RE.test(best.pairAddress || "")) return null;
  const q = ((best.quoteToken && best.quoteToken.symbol) || "").toUpperCase();
  if (q !== "SOL" && q !== "WSOL" && q !== "USDC") return null;
  return { pool: best.pairAddress, quote: q === "USDC" ? "USDC" : "SOL" };
}

// Annotate the top survivors with a dynamically wireable pool. Best-effort per
// token: any fetch/parse failure leaves the row pool-less (REST-priced) — the
// scan itself never fails because of enrichment.
async function annotatePools(survivors, maxN) {
  for (let i = 0; i < Math.min(survivors.length, maxN); i++) {
    if (i > 0) await sleep(250);
    const s = survivors[i];
    try {
      const res = await fetch(`https://api.dexscreener.com/latest/dex/tokens/${s.mint}`, {
        headers: { accept: "application/json" },
      });
      if (!res.ok) continue;
      const body = await res.json();
      const picked = pickPumpswapPool((body && body.pairs) || []);
      if (picked) {
        s.pool = picked.pool;
        s.quote = picked.quote;
      } else {
        console.error(`  scan: ${s.symbol} best pool not pumpswap SOL/USDC — REST-priced`);
      }
    } catch (_) { /* REST-priced */ }
  }
  return survivors;
}
```

In `main()`, directly after `survivors = rankSurvivors(survivors, OPTS);`:

```js
  if (OPTS.poolEnrichMax > 0) {
    await annotatePools(survivors, OPTS.poolEnrichMax);
  }
```

Add `pickPumpswapPool` to `module.exports`. Add to the header Env doc line:
`SCAN_POOL_ENRICH_MAX (5; top-N survivors get a DexScreener best-pool lookup — pumpswap pools are emitted as pool/quote for dynamic gRPC wiring; 0 = off).`

- [ ] **Step 4: Run tests to verify they pass**

Run: `node --test scripts/scan_tokens.test.js`
Expected: all pass (25 = 21 existing + 4 new)

- [ ] **Step 5: Live smoke** (network, non-fatal if market is quiet):

Run: `node scripts/scan_tokens.js 2>&1 | head -20`
Expected: no crash; any kept survivor prints normally (pool fields only visible in `--json`).

- [ ] **Step 6: Commit**

```bash
git add scripts/scan_tokens.js scripts/scan_tokens.test.js
git commit -m "feat(scan): emit best pumpswap pool per survivor for dynamic gRPC wiring"
```

---

### Task 2: `ScanCandidate` carries pool/quote into `WatchedToken`

**Files:**
- Modify: `src/portfolio/watcher.rs` (`ScanCandidate` ~line 1120, `run_token_scan` ~line 1129)
- Test: `src/portfolio/watcher.rs` `#[cfg(test)]` block (~line 1588, next to `scan_candidate_parses_and_take_n_maps_to_watched`)

**Interfaces:**
- Consumes: Task 1's JSON fields `pool`/`quote` (both optional strings).
- Produces: `fn candidates_to_watched(cands: Vec<ScanCandidate>, top_n: usize) -> Vec<WatchedToken>` — discovered `WatchedToken`s now carry `pool: Option<String>, quote: Option<String>`; Task 6 reads `w.pool` on discoveries.

- [ ] **Step 1: Write the failing test** — add to the `#[cfg(test)]` module:

```rust
    #[test]
    fn scan_candidate_carries_pool_and_quote_into_watched() {
        let json = r#"[
            {"symbol":"AAA","mint":"mAAA","name":"Alpha","pool":"pAAA","quote":"SOL","vol24":9.0},
            {"symbol":"BBB","mint":"mBBB"}
        ]"#;
        let cands: Vec<ScanCandidate> = serde_json::from_str(json).unwrap();
        let w = candidates_to_watched(cands, 5);
        assert_eq!(w[0].pool.as_deref(), Some("pAAA"));
        assert_eq!(w[0].quote.as_deref(), Some("SOL"));
        assert_eq!(w[1].pool, None, "pool-less rows stay REST-priced");
        assert_eq!(w[1].quote, None);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib scan_candidate_carries_pool -- --nocapture`
Expected: FAIL — `candidates_to_watched` not found / no `pool` field on `ScanCandidate`

- [ ] **Step 3: Implement** — extend the struct:

```rust
#[derive(Debug, serde::Deserialize)]
struct ScanCandidate {
    symbol: String,
    mint: String,
    #[serde(default)]
    name: Option<String>,
    /// PumpSwap pool + quote side from scan_tokens.js pool enrichment — present only
    /// when the token's best venue is dynamically wireable (spec 2026-07-22).
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    quote: Option<String>,
}
```

Extract the mapping out of `run_token_scan` into a pure function (and call it from `run_token_scan` in place of the inline `.map(...)`):

```rust
/// Pure mapping half of `run_token_scan` (unit-tested): top-`top_n` scan rows →
/// watch entries, carrying the wireable pool/quote when the scanner emitted one.
fn candidates_to_watched(cands: Vec<ScanCandidate>, top_n: usize) -> Vec<WatchedToken> {
    cands
        .into_iter()
        .take(top_n)
        .map(|c| WatchedToken {
            symbol: c.symbol,
            mint: c.mint,
            name: c.name,
            equity: None,
            params: None,
            pool: c.pool,
            quote: c.quote,
            pools: None,
        })
        .collect()
}
```

In `run_token_scan`, replace the final `Ok(cands.into_iter().take(top_n).map(...).collect())` with `Ok(candidates_to_watched(cands, top_n))`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib scan_candidate -- --nocapture`
Expected: both `scan_candidate_*` tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/watcher.rs
git commit -m "feat(watcher): scan candidates carry pumpswap pool/quote into WatchedToken"
```

---

### Task 3: Move the feed bootstrap into the lib (`portfolio::feed_setup`), return the task handle

**Files:**
- Create: `src/portfolio/feed_setup.rs`
- Modify: `src/bin/portfolio_watcher.rs` (remove moved items; adapt 2 call sites: main ~line 659, grpc-smoke ~line 579), `src/portfolio/mod.rs` (add `pub mod feed_setup;`)

**Interfaces:**
- Produces: `pub async fn feed_setup::spawn_grpc_feed(cfg: &PortfolioConfig, watched: &[WatchedToken]) -> anyhow::Result<Option<(GrpcFeed, tokio::task::JoinHandle<()>)>>` — Tasks 4 and 6 depend on this exact signature (Task 4 adds a third parameter).
- No behavior change: pure move + handle capture (dropping a `JoinHandle` detaches, same as today's `tokio::spawn`).

- [ ] **Step 1: Create the module** — move these items VERBATIM from `src/bin/portfolio_watcher.rs` into new `src/portfolio/feed_setup.rs` (current bin line anchors: `enum Role` ~47, `struct WiredPool` ~56, `spawn_grpc_feed` ~96–274, `apply_update` ~281, the reprice helper ~322, the reseed helper ~399, `run_grpc_stream` ~482 to its end, plus any small private helpers only they use — follow compiler errors):

Module header + imports for the new file:

```rust
//! gRPC price-feed bootstrap: resolve (watched token × pool) pairs into vault/state
//! subscriptions and spawn the Yellowstone stream task. Lives in the lib (not the
//! watcher bin) so the runtime scan handler can RE-spawn the feed when dynamically
//! discovered pools change (spec 2026-07-22-grpc-priced-scan-discoveries).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{error, info, warn};

use crate::dex;
use crate::portfolio::grpc_pricer::GrpcFeed;
use crate::portfolio::momentum_universe::WatchedToken;
use crate::portfolio::{scanner, PortfolioConfig};
```

(Adjust `use` paths from the bin's `solana_mev::…` form to `crate::…`; the compiler drives the exact list.)

- [ ] **Step 2: Capture and return the stream-task handle** — in the moved `spawn_grpc_feed`, change the tail:

```rust
    let handle = tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match run_grpc_stream(&endpoint, token.as_deref(), &accounts, &acct_index, &wired, &feed_task, &rpc_url).await {
                Ok(()) => warn!("gRPC price stream closed — reconnecting in {}s", backoff.as_secs()),
                Err(e) => error!("gRPC price stream error: {e} — reconnecting in {}s", backoff.as_secs()),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
    Ok(Some((feed, handle)))
```

and the signature to:

```rust
pub async fn spawn_grpc_feed(
    cfg: &PortfolioConfig,
    watched: &[WatchedToken],
) -> Result<Option<(GrpcFeed, tokio::task::JoinHandle<()>)>> {
```

Every early `return Ok(None)` stays as-is. Register the module in `src/portfolio/mod.rs`: `pub mod feed_setup;`.

- [ ] **Step 3: Adapt the two bin call sites** — in `src/bin/portfolio_watcher.rs`, delete the moved items, add `use solana_mev::portfolio::feed_setup::spawn_grpc_feed;`, then:

main (~line 659):

```rust
    let grpc_feed = match spawn_grpc_feed(&cfg, &watched).await {
        Ok(feed) => feed, // Option<(GrpcFeed, JoinHandle<()>)> — run() takes it whole from Task 6 on; for now:
        Err(e) => { warn!("gRPC feed setup failed ({e}) — REST only"); None }
    };
    portfolio::watcher::run(cfg, http, grpc_feed.map(|(f, _task)| f)).await;
```

grpc-smoke (~line 579):

```rust
    let Some((feed, _task)) = spawn_grpc_feed(&cfg, &watched).await? else {
```

- [ ] **Step 4: Build + full lib test to verify zero regression**

Run: `cargo build --release 2>&1 | tail -3 && cargo test --lib 2>&1 | tail -3`
Expected: builds clean (no new warnings), all lib tests pass

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/feed_setup.rs src/portfolio/mod.rs src/bin/portfolio_watcher.rs
git commit -m "refactor(watcher): move gRPC feed bootstrap into lib, return stream task handle"
```

---

### Task 4: `extra_pools` parameter + curated-wins merge

**Files:**
- Modify: `src/portfolio/feed_setup.rs` (`spawn_grpc_feed` signature + `by_id` construction)
- Test: `src/portfolio/feed_setup.rs` new `#[cfg(test)]` block

**Interfaces:**
- Consumes: `dex::types::PoolConfig` (Clone + Deserialize, `id: String` field).
- Produces: `pub fn merge_pool_configs(from_file: Vec<PoolConfig>, extra: Vec<PoolConfig>) -> HashMap<String, PoolConfig>`; `spawn_grpc_feed(cfg, watched, extra_pools: &[PoolConfig])` — Task 6 passes decoded configs here; all existing call sites pass `&[]`.

- [ ] **Step 1: Write the failing test** — in `src/portfolio/feed_setup.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn pc(id: &str, token_a: &str) -> crate::dex::types::PoolConfig {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "dex": "pump_swap",
            "token_a": token_a,
            "token_b": "So11111111111111111111111111111111111111112",
            "vault_a": "va", "vault_b": "vb",
            "fee_bps": 25
        }))
        .expect("minimal PoolConfig")
    }

    #[test]
    fn merge_pool_configs_curated_wins_on_collision() {
        let curated = vec![pc("P1", "curatedMint")];
        let extra = vec![pc("P1", "scanMint"), pc("P2", "scanOnly")];
        let merged = merge_pool_configs(curated, extra);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged["P1"].token_a, "curatedMint", "pools.json entry must win");
        assert_eq!(merged["P2"].token_a, "scanOnly", "extra-only pool survives");
    }
}
```

(If `PoolConfig` requires more mandatory fields, copy the smallest real `pump_swap` entry from `pools.json` into the `json!` literal — the test asserts only `len` and `token_a`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib merge_pool_configs -- --nocapture`
Expected: FAIL — `merge_pool_configs` not found

- [ ] **Step 3: Implement** — in `feed_setup.rs`:

```rust
/// Merge ad-hoc decoded pool configs UNDER the pools.json set: on id collision the
/// pools.json entry wins — curated wiring is authoritative, a scan must never
/// re-route a curated token's pricing.
pub fn merge_pool_configs(
    from_file: Vec<dex::types::PoolConfig>,
    extra: Vec<dex::types::PoolConfig>,
) -> HashMap<String, dex::types::PoolConfig> {
    let mut map: HashMap<String, dex::types::PoolConfig> =
        extra.into_iter().map(|c| (c.id.clone(), c)).collect();
    for c in from_file {
        map.insert(c.id.clone(), c);
    }
    map
}
```

Change the signature to `pub async fn spawn_grpc_feed(cfg: &PortfolioConfig, watched: &[WatchedToken], extra_pools: &[dex::types::PoolConfig]) -> Result<Option<(GrpcFeed, tokio::task::JoinHandle<()>)>>` and replace the `by_id` construction:

```rust
    let merged = merge_pool_configs(configs, extra_pools.to_vec());
    let by_id: HashMap<&str, &dex::types::PoolConfig> =
        merged.iter().map(|(k, v)| (k.as_str(), v)).collect();
```

Update the two bin call sites to pass `&[]`.

- [ ] **Step 4: Run tests + build**

Run: `cargo test --lib merge_pool_configs && cargo build --release 2>&1 | tail -2`
Expected: PASS, clean build

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/feed_setup.rs src/bin/portfolio_watcher.rs
git commit -m "feat(feed): spawn_grpc_feed accepts extra ad-hoc pool configs (curated wins)"
```

---

### Task 5: Ad-hoc pool decode runner (`run_pool_decode`)

**Files:**
- Modify: `src/portfolio/watcher.rs` (new helpers next to `run_token_scan` ~line 1129)
- Test: `src/portfolio/watcher.rs` `#[cfg(test)]` block

**Interfaces:**
- Consumes: `scripts/fetch_pumpswap_pools.js --pools <a,b> --output <file>` (existing ad-hoc mode; on-chain decode + vault↔mint cross-check; writes a JSON array of PoolConfig).
- Produces: `async fn run_pool_decode(script: &str, pools: &[String]) -> anyhow::Result<Vec<PoolConfig>>`; pure `fn parse_pool_configs(raw: &str) -> anyhow::Result<Vec<PoolConfig>>`; `const POOL_DECODE_SCRIPT: &str = "scripts/fetch_pumpswap_pools.js";` — Task 6 calls `run_pool_decode(POOL_DECODE_SCRIPT, …)`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn parse_pool_configs_reads_decoder_output() {
        let raw = r#"[{
            "id": "BkPool111111111111111111111111111111111111",
            "dex": "pump_swap",
            "token_a": "So11111111111111111111111111111111111111112",
            "token_b": "mTOK",
            "vault_a": "va", "vault_b": "vb",
            "fee_bps": 25
        }]"#;
        let configs = parse_pool_configs(raw).expect("decoder-shaped JSON parses");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].id, "BkPool111111111111111111111111111111111111");
        assert!(parse_pool_configs("not json").is_err());
    }
```

(As in Task 4: if `PoolConfig` needs more mandatory fields, extend the literal from a real `pools.json` pump_swap entry.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib parse_pool_configs -- --nocapture`
Expected: FAIL — `parse_pool_configs` not found

- [ ] **Step 3: Implement** — next to `run_token_scan` in `watcher.rs`:

```rust
/// The existing PumpSwap decoder script (ad-hoc `--pools` mode). Relative to the
/// bot's working directory, like `MOMENTUM_SCAN_SCRIPT`'s default.
const POOL_DECODE_SCRIPT: &str = "scripts/fetch_pumpswap_pools.js";

/// Pure parse half of `run_pool_decode` (unit-tested): the decoder writes a JSON
/// array in the PoolConfig schema.
fn parse_pool_configs(raw: &str) -> anyhow::Result<Vec<crate::dex::types::PoolConfig>> {
    serde_json::from_str(raw).context("pool decoder output was not a PoolConfig array")
}

/// Decode PumpSwap pool accounts for dynamically discovered tokens by spawning the
/// existing JS decoder (on-chain layout decode + mandatory vault↔mint cross-check).
/// Any failure is an Err — the caller keeps the previous feed (REST fallback), and
/// the next scan tick retries naturally.
async fn run_pool_decode(
    script: &str,
    pools: &[String],
) -> anyhow::Result<Vec<crate::dex::types::PoolConfig>> {
    let tmp = std::env::temp_dir().join(format!("scan_pools_{}.json", std::process::id()));
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("node")
            .arg(script)
            .arg("--pools")
            .arg(pools.join(","))
            .arg("--output")
            .arg(&tmp)
            .output(),
    )
    .await
    .context("pool decode timed out after 30s")?
    .with_context(|| format!("failed to spawn `node {script} --pools …`"))?;
    if !out.status.success() {
        anyhow::bail!(
            "pool decode exited {}: {}",
            out.status.code().map_or_else(|| "signal".to_string(), |c| c.to_string()),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = std::fs::read_to_string(&tmp).with_context(|| format!("reading {}", tmp.display()))?;
    let _ = std::fs::remove_file(&tmp);
    parse_pool_configs(&raw)
}
```

- [ ] **Step 4: Run tests + build**

Run: `cargo test --lib parse_pool_configs && cargo build --release 2>&1 | tail -2`
Expected: PASS, clean build (`run_pool_decode` may warn unused until Task 6 — silence with `#[allow(dead_code)]` REMOVED again in Task 6, or land Tasks 5+6 in one review if preferred)

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/watcher.rs
git commit -m "feat(watcher): ad-hoc pumpswap pool decode via existing JS decoder"
```

---

### Task 6: Scan-arm integration — differ, re-spawn, swap

**Files:**
- Modify: `src/portfolio/watcher.rs` (`run()` signature ~line 31, `spike_rx` init ~line 395, scan arm ~line 581, new pure fn + state var), `src/bin/portfolio_watcher.rs` (main call site passes the tuple through)
- Test: `src/portfolio/watcher.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 2 (`w.pool` on discoveries), Task 4 (`feed_setup::spawn_grpc_feed(cfg, watched, extra)`), Task 5 (`run_pool_decode`), existing `effective_universe`, `held_mints_from_state`, locals `watched` (curated, line 63) and `discovered` (line 297).
- Produces: `pub async fn run(cfg, http, grpc_feed: Option<(GrpcFeed, tokio::task::JoinHandle<()>)>)`; pure `fn dynamic_pool_set(discovered: &[WatchedToken]) -> std::collections::HashSet<String>`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn dynamic_pool_set_collects_only_pooled_discoveries() {
        let mut a = wt("AAA", "mAAA");
        a.pool = Some("pAAA".into());
        let b = wt("BBB", "mBBB"); // pool-less — REST
        let set = dynamic_pool_set(&[a, b]);
        assert_eq!(set.len(), 1);
        assert!(set.contains("pAAA"));
        assert!(dynamic_pool_set(&[]).is_empty());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib dynamic_pool_set -- --nocapture`
Expected: FAIL — `dynamic_pool_set` not found

- [ ] **Step 3: Implement the pure fn**

```rust
/// Pool ids of discoveries that carry a dynamically wireable pool — the change
/// signal for the feed re-spawn (set-compared against what is currently wired).
fn dynamic_pool_set(discovered: &[WatchedToken]) -> HashSet<String> {
    discovered.iter().filter_map(|w| w.pool.clone()).collect()
}
```

- [ ] **Step 4: Thread the task handle through `run()`** — change the signature and unpack:

```rust
pub async fn run(
    cfg: PortfolioConfig,
    http: Client,
    grpc_feed: Option<(GrpcFeed, tokio::task::JoinHandle<()>)>,
) {
    let (mut grpc_feed, mut feed_task): (Option<GrpcFeed>, Option<tokio::task::JoinHandle<()>>) =
        match grpc_feed {
            Some((f, h)) => (Some(f), Some(h)),
            None => (None, None),
        };
```

`spike_rx` (line ~395) becomes `let mut spike_rx = …` (it already is `mut`). Everything else in `run()` keeps reading `grpc_feed.as_ref()` unchanged. In the bin main, revert Task 3's `.map(|(f, _task)| f)` shim:

```rust
    portfolio::watcher::run(cfg, http, grpc_feed).await;
```

- [ ] **Step 5: Add the swap logic in the scan arm** — inside the `Ok(found)` branch, AFTER the existing `discovered = found; …` block (and its `token_mints` fold), add — with `let mut wired_dynamic: HashSet<String> = HashSet::new();` declared next to `let mut discovered` (~line 297):

```rust
                        // Dynamic gRPC wiring (spec 2026-07-22): discoveries carrying a
                        // pumpswap pool get vault subscriptions by re-spawning the feed
                        // with their ad-hoc decoded PoolConfigs merged in. pools.json is
                        // never written; unchanged pool set → no rebuild (the common
                        // case: the same top-N rediscovered hourly).
                        if cfg.momentum_grpc_pricing {
                            let want = dynamic_pool_set(&discovered);
                            if want != wired_dynamic {
                                let pool_ids: Vec<String> = want.iter().cloned().collect();
                                let decoded = if pool_ids.is_empty() {
                                    Ok(Vec::new())
                                } else {
                                    run_pool_decode(POOL_DECODE_SCRIPT, &pool_ids).await
                                };
                                match decoded {
                                    Ok(extra) => {
                                        let universe = effective_universe(
                                            &watched, &discovered, &held_mints_from_state(&cfg),
                                        );
                                        match crate::portfolio::feed_setup::spawn_grpc_feed(
                                            &cfg, &universe, &extra,
                                        )
                                        .await
                                        {
                                            Ok(Some((new_feed, new_task))) => {
                                                if let Some(old) = feed_task.take() {
                                                    old.abort();
                                                }
                                                spike_rx = new_feed
                                                    .spike_rx
                                                    .lock()
                                                    .ok()
                                                    .and_then(|mut g| g.take());
                                                grpc_feed = Some(new_feed);
                                                feed_task = Some(new_task);
                                                wired_dynamic = want;
                                                info!(
                                                    "gRPC feed re-spawned with {} dynamic pool(s)",
                                                    extra.len()
                                                );
                                            }
                                            Ok(None) => warn!(
                                                "gRPC feed re-spawn produced no feed — keeping previous"
                                            ),
                                            Err(e) => warn!(
                                                "gRPC feed re-spawn failed ({e}) — keeping previous"
                                            ),
                                        }
                                    }
                                    Err(e) => warn!(
                                        "scan pool decode failed ({e}) — discoveries stay REST"
                                    ),
                                }
                            }
                        }
```

(If the borrow checker rejects reassigning `grpc_feed`/`spike_rx` inside the select arm because another arm borrows them, hoist the swap: set a `pending_swap: Option<(GrpcFeed, JoinHandle<()>, HashSet<String>)>` in the arm and apply it at the top of the loop body before `tokio::select!` — same semantics, borrow-clean.)

- [ ] **Step 6: Full build + tests**

Run: `cargo build --release 2>&1 | tail -2 && cargo test --lib 2>&1 | tail -3`
Expected: clean build, all tests pass (including the 4 new ones from Tasks 2/4/5/6)

- [ ] **Step 7: End-to-end smoke**

Run: `cargo run --release --bin portfolio-watcher -- grpc-smoke` (or the repo's documented smoke invocation if it differs — check `main()`'s subcommand parsing)
Expected: `gRPC smoke: PASS` — proves the moved/parameterized bootstrap still streams.

- [ ] **Step 8: Commit**

```bash
git add src/portfolio/watcher.rs src/bin/portfolio_watcher.rs
git commit -m "feat(watcher): re-spawn gRPC feed with decoded pools when scan discoveries change"
```

---

### Task 7: Documentation

**Files:**
- Modify: `CLAUDE.md` (the "Live token discovery" bullet under Strategy research)

**Interfaces:** none (docs only).

- [ ] **Step 1: Update the discovery bullet** — append to the `MOMENTUM_SCAN_ENABLE` paragraph in `CLAUDE.md`:

```markdown
  Since 2026-07-22 the scanner also emits each survivor's best **PumpSwap** pool
  (`SCAN_POOL_ENRICH_MAX`, default 5; DexScreener highest-24h-volume rule) and the
  watcher **gRPC-wires discoveries dynamically**: on a changed discovered-pool set it
  decodes the pools ad-hoc (`fetch_pumpswap_pools.js --pools …`, vault↔mint
  cross-checked) and re-spawns the price feed with them merged in (curated pools.json
  entries win on collision; pools.json is never written; non-PumpSwap-venue
  discoveries stay REST-priced). Also since 2026-07-22 the scanner rejects
  concentrated supply (`SCAN_MAX_TOP_HOLDERS_PCT`, default 30) and ranks by
  `MOMENTUM_SCAN_CHANGE_WINDOW` (set `4h` to match the trader's return metric).
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: dynamic gRPC wiring + safety/horizon knobs for the momentum scanner"
```

---

## Self-Review (done at plan time)

- **Spec coverage:** §1 scanner → Task 1; §2 ad-hoc decode → Task 5; §3 re-spawn/merge/swap → Tasks 3+4+6; §4 failure table → encoded in Tasks 5/6 error arms + Task 1 fail-open; §5 testing → each task's test steps + Task 6 smoke. Rollout (dark ship) needs no task.
- **Placeholder scan:** none — every code step shows the code; the only soft references are compiler-driven import lists in the verbatim move (Task 3), which is the correct instruction for a move.
- **Type consistency:** `spawn_grpc_feed` tuple return introduced in Task 3, threaded in Task 6; `extra_pools: &[PoolConfig]` introduced Task 4, consumed Task 6; JSON field names `pool`/`quote` identical in Tasks 1/2.
