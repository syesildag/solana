//! The watched-token universe for the momentum trader: a hand-curated JSON list
//! (`assets/momentum_tokens.json`), distinct from `portfolio.json` (which is the
//! auto-generated holdings snapshot). Each entry is `{ "symbol", "mint" }`.

use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

/// USDC — the cash leg. Never momentum-traded; both sides of every swap touch it.
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const USDC_DECIMALS: u8 = 6;

/// Optional per-token momentum parameter overrides. Each field falls back to the
/// global `.env` value when `None`. Only token-specific knobs are overridable;
/// metric/lookback/rotate stay global. `regime_filter: Some(false)` exempts a
/// token from the global SOL regime gate (it may enter even when the market is
/// risk-off); `None` (the default) means "obey the global gate". The overbought
/// entry gate is overridable as a pair: `entry_max_z_obs: Some(0)` disables the
/// gate for that token; a non-zero window uses `entry_max_z` (or the global z
/// threshold when only the window is overridden).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_metric: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trail_pct: Option<f64>,
    /// Initial-risk stop (percent below entry), active ONLY until the position first trades
    /// above entry; then `trail_pct` governs. Fills the gap where `exit_on_fade` (which
    /// requires green) leaves a never-green entry riding the full trail. None/0 = off.
    pub initial_stop_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regime_filter: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade_usdc: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_on_fade: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reentry_cooldown_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_max_z_obs: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_max_z: Option<f64>,
    /// Per-token ranking lookback window (observations), overriding the global
    /// `MOMENTUM_LOOKBACK_OBS`. Metric AND lookback are otherwise global; this lets a
    /// token whose edge lives at a different horizon (e.g. an LST that ranks best over
    /// 720 obs while pump names use 480) carry its own window. Must exceed
    /// `SORTINO_MIN_OBS` (120) or the token simply never ranks (its metrics can't
    /// compute) — same silent warm-up floor as the global knob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback_obs: Option<usize>,
    /// Regime-death exit, for a token that IS the regime asset (an LST: JitoSOL ≡ SOL).
    /// Exit an UNDERWATER position once the SOL trend regime has been continuously OFF for
    /// this many observations. For such a token the entry premise (SOL clean uptrend) is the
    /// position thesis itself, so when the premise dies while the position is red, the reason
    /// for holding is gone — and because the regime gate blocks all NEW entries while off,
    /// exiting to cash then has zero opportunity cost by construction. Do NOT set this on an
    /// idiosyncratic token (ZEC/HYPE/…): there the SOL regime is a foreign signal and the
    /// same rule measured −$946 across the book. None/0 = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regime_exit_obs: Option<usize>,

    // ----- order-flow entry gate (see `portfolio::flow`) -----
    /// Absolute 1h-volume floor in USD; below it, no entry. `None`/0 = off. Usually leave
    /// off in favour of `min_vol_decay` — an absolute floor punishes a natively quiet deep
    /// pool (JitoSOL trades ~$12k/h and is perfectly healthy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_vol_h1_usd: Option<f64>,
    /// Require 1h volume ≥ this multiple of the token's OWN 24h hourly average. Scale-free,
    /// so one value works across a $4.8M pool and a $460k one. Measured 2026-08-01 the book
    /// sat at 0.55–1.32, so 0.3 leaves headroom while still catching a real collapse.
    /// `None`/0 = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_vol_decay: Option<f64>,
    /// Veto entry when sells-per-buy over 1h exceeds this AND the price is rising —
    /// distribution into strength. Only acts above `min_txns_h1`. Baseline across the book
    /// is 1.3–3.2, so ~5.0 is the outlier line. `None`/0 = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sell_buy_ratio: Option<f64>,
    /// Minimum 1h transaction count before `max_sell_buy_ratio` may fire. This is a GUARD,
    /// not a gate: JitoSOL logged 67 sells against **one** buy in an hour while rising, and
    /// without this floor that reads as extreme distribution on the healthiest token in the
    /// book. Defaults to the global (200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_txns_h1: Option<u64>,
}

/// One venue (pool + quote) to price a watched token from gRPC. A single `WatchedToken`
/// may carry several of these (`pools`, Task 3 schema) so a listing that trades on
/// multiple DEXes can be priced from more than one on-chain source; the ingestion side
/// treats every ref as an independent subscription and the shared price map is last-
/// write-wins across all of them (see `apply_update` in `portfolio_watcher.rs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolRef {
    pub pool: String,
    pub quote: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchedToken {
    pub symbol: String,
    pub mint: String,
    /// Human-readable name (e.g. "Broadcom xStock"), used in trade emails/logs.
    /// Optional — entries written by the add-token script include it.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether this token follows equity market hours (so the closed-market guard
    /// applies). `None` ⇒ auto-detect from the name (tokenized stocks/ETFs); set
    /// explicitly to override. 24/7 crypto stays `false` and is never frozen-out.
    #[serde(default)]
    pub equity: Option<bool>,
    /// Optional per-token parameter overrides (min_metric/trail_pct/max_run_pct/
    /// entry_max_z gate/…); each falls back to the global config when absent. See
    /// `TokenParams`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<TokenParams>,
    /// Optional Raydium/Meteora/Orca pool pubkey for gRPC pricing (Task 1 schema).
    /// Single-venue shorthand — superseded by `pools` when that is present. See
    /// `pool_refs()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// Optional quote token mint for normalized pricing (Task 1 schema). Pairs with
    /// `pool` above; ignored when `pools` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    /// Optional list of pools for multi-venue gRPC pricing (Task 3 schema). When
    /// present, wins outright over the single `pool`+`quote` shorthand (not merged —
    /// see `pool_refs()`; `load()` warns once if both are set on the same entry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pools: Option<Vec<PoolRef>>,
}

impl WatchedToken {
    /// Does this token follow market hours (vs trade 24/7)? Explicit `equity`
    /// wins; otherwise inferred from the name (Backed xStocks are "… xStock",
    /// Ondo tokenized equities contain "ondo").
    pub fn is_equity(&self) -> bool {
        self.equity.unwrap_or_else(|| {
            self.name
                .as_deref()
                .map(|n| {
                    let n = n.to_lowercase();
                    n.contains("xstock") || n.contains("ondo")
                })
                .unwrap_or(false)
        })
    }

    /// The venues to price this token from via gRPC: the `pools` list if present
    /// (multi-venue), else the single `pool`+`quote` shorthand as a one-element vec,
    /// else empty (REST-only — no pool configured).
    pub fn pool_refs(&self) -> Vec<PoolRef> {
        if let Some(pools) = &self.pools {
            return pools.clone();
        }
        match (self.pool.as_deref(), self.quote.as_deref()) {
            (Some(pool), Some(quote)) => {
                vec![PoolRef { pool: pool.to_string(), quote: quote.to_string() }]
            }
            _ => Vec::new(),
        }
    }
}

/// Load and validate the watched universe. Entries with an unparseable mint are
/// dropped (with a warning), the list is deduped by mint, and USDC is removed if
/// present. An empty/maformed file is an error so misconfiguration surfaces at
/// startup rather than silently disabling the trader.
pub fn load(path: &Path) -> Result<Vec<WatchedToken>> {
    let data = std::fs::read_to_string(path)
        .with_context(|| format!("could not read momentum tokens file {}", path.display()))?;
    let raw: Vec<WatchedToken> = serde_json::from_str(&data)
        .context("momentum tokens file must be a JSON array of {symbol, mint}")?;

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for w in raw {
        if w.mint == USDC_MINT {
            continue; // never momentum-trade the cash leg
        }
        if Pubkey::from_str(&w.mint).is_err() {
            tracing::warn!(
                "momentum universe: skipping {} — '{}' is not a valid mint",
                w.symbol,
                w.mint
            );
            continue;
        }
        if w.pools.is_some() && (w.pool.is_some() || w.quote.is_some()) {
            tracing::warn!(
                "momentum universe: {} sets both 'pools' and the single 'pool'/'quote' shorthand — 'pools' wins",
                w.symbol
            );
        }
        if seen.insert(w.mint.clone()) {
            out.push(w);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: &str = "So11111111111111111111111111111111111111112";
    const RAY: &str = "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R";

    fn write_tokens(json: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("momentum_tokens_{}.json", rand::random::<u32>()));
        std::fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn loads_dedups_drops_usdc_and_invalid() {
        let path = write_tokens(&format!(
            r#"[
                {{"symbol":"SOL","mint":"{SOL}"}},
                {{"symbol":"RAY","mint":"{RAY}"}},
                {{"symbol":"RAY-dup","mint":"{RAY}"}},
                {{"symbol":"USDC","mint":"{USDC_MINT}"}},
                {{"symbol":"BAD","mint":"not-a-pubkey"}}
            ]"#
        ));
        let got = load(&path).unwrap();
        let mints: Vec<&str> = got.iter().map(|w| w.mint.as_str()).collect();
        assert_eq!(got.len(), 2, "USDC dropped, dup removed, invalid skipped");
        assert!(mints.contains(&SOL));
        assert!(mints.contains(&RAY));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn is_equity_classifies_by_name_with_override() {
        let tok = |name: Option<&str>, equity: Option<bool>| WatchedToken {
            symbol: "X".into(),
            mint: "M".into(),
            name: name.map(String::from),
            equity,
            params: None,
            pool: None,
            quote: None,
            pools: None,
        };
        // Auto-detected from the name:
        assert!(tok(Some("Broadcom xStock"), None).is_equity());
        assert!(tok(Some("Apple (Ondo Tokenized)"), None).is_equity());
        assert!(!tok(Some("Jito Staked SOL"), None).is_equity(), "LST trades 24/7");
        assert!(!tok(Some("Meteora"), None).is_equity());
        assert!(!tok(None, None).is_equity(), "unknown ⇒ 24/7");
        // Explicit override wins either way:
        assert!(tok(Some("Jito Staked SOL"), Some(true)).is_equity());
        assert!(!tok(Some("Broadcom xStock"), Some(false)).is_equity());
    }

    #[test]
    fn malformed_file_errors() {
        let path = write_tokens("{ not an array }");
        assert!(load(&path).is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parses_per_token_params_full_partial_and_absent() {
        let json = r#"[
          {"symbol":"AAA","mint":"So11111111111111111111111111111111111111112",
           "params":{"min_metric":0.05,"trail_pct":30.0,"max_run_pct":0.0}},
          {"symbol":"BBB","mint":"BPxxfRCXkUVhig4HS1Lh7kZqV6SPJhzfEk4x6fVBjPCy",
           "params":{"trail_pct":12.0}},
          {"symbol":"CCC","mint":"jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL"}
        ]"#;
        let raw: Vec<WatchedToken> = serde_json::from_str(json).unwrap();
        // full
        let a = raw[0].params.as_ref().unwrap();
        assert_eq!(a.min_metric, Some(0.05));
        assert_eq!(a.trail_pct, Some(30.0));
        assert_eq!(a.max_run_pct, Some(0.0));
        // partial — only trail set, others None (per-field fallback)
        let b = raw[1].params.as_ref().unwrap();
        assert_eq!(b.trail_pct, Some(12.0));
        assert_eq!(b.min_metric, None);
        assert_eq!(b.max_run_pct, None);
        // absent — no params block
        assert!(raw[2].params.is_none());
    }

    #[test]
    fn token_params_parse_regime_filter() {
        let json = r#"[{"symbol":"A","mint":"A","params":{"regime_filter":false}},
                       {"symbol":"B","mint":"B","params":{"min_metric":0.05}},
                       {"symbol":"C","mint":"C"}]"#;
        let v: Vec<WatchedToken> = serde_json::from_str(json).unwrap();
        assert_eq!(v[0].params.as_ref().unwrap().regime_filter, Some(false)); // exempt
        assert_eq!(v[1].params.as_ref().unwrap().regime_filter, None);        // field absent
        assert!(v[2].params.is_none());                                       // no params
    }

    #[test]
    fn token_params_parse_extended_fields() {
        let json = r#"[{"symbol":"A","mint":"A","params":{"trade_usdc":250.0,"exit_on_fade":false,"reentry_cooldown_secs":1800,"entry_max_z_obs":0,"entry_max_z":2.0}},
                       {"symbol":"B","mint":"B","params":{"min_metric":0.05}},
                       {"symbol":"C","mint":"C"}]"#;
        let v: Vec<WatchedToken> = serde_json::from_str(json).unwrap();
        let a = v[0].params.as_ref().unwrap();
        assert_eq!(a.trade_usdc, Some(250.0));
        assert_eq!(a.exit_on_fade, Some(false));
        assert_eq!(a.reentry_cooldown_secs, Some(1800));
        assert_eq!(a.entry_max_z_obs, Some(0)); // Some(0) = gate disabled for this token
        assert_eq!(a.entry_max_z, Some(2.0));
        let b = v[1].params.as_ref().unwrap();
        assert_eq!((b.trade_usdc, b.exit_on_fade, b.reentry_cooldown_secs), (None, None, None));
        assert_eq!((b.entry_max_z_obs, b.entry_max_z), (None, None));
        assert!(v[2].params.is_none());
    }

    #[test]
    fn token_params_parse_lookback_obs() {
        let json = r#"[{"symbol":"A","mint":"A","params":{"lookback_obs":720}},
                       {"symbol":"B","mint":"B","params":{"min_metric":0.05}},
                       {"symbol":"C","mint":"C"}]"#;
        let v: Vec<WatchedToken> = serde_json::from_str(json).unwrap();
        assert_eq!(v[0].params.as_ref().unwrap().lookback_obs, Some(720));
        assert_eq!(v[1].params.as_ref().unwrap().lookback_obs, None); // field absent → global
        assert!(v[2].params.is_none());
    }

    #[test]
    fn token_without_params_serializes_without_the_key() {
        let w = WatchedToken {
            symbol: "AAA".into(),
            mint: "So11111111111111111111111111111111111111112".into(),
            name: None,
            equity: None,
            params: None,
            pool: None,
            quote: None,
            pools: None,
        };
        let s = serde_json::to_string(&w).unwrap();
        assert!(!s.contains("params"), "no params key when None, got: {s}");
    }

    #[test]
    fn watched_token_pool_quote_optional_roundtrip() {
        // entry WITHOUT pool/quote (back-compat) deserializes with None
        let legacy: WatchedToken = serde_json::from_str(
            r#"{"symbol":"MET","mint":"METxxxx","name":"Meteora"}"#).unwrap();
        assert!(legacy.pool.is_none() && legacy.quote.is_none());
        // entry WITH pool/quote
        let withpool: WatchedToken = serde_json::from_str(
            r#"{"symbol":"BP","mint":"BPxxxx","pool":"PoolPubkey","quote":"USDC"}"#).unwrap();
        assert_eq!(withpool.pool.as_deref(), Some("PoolPubkey"));
        assert_eq!(withpool.quote.as_deref(), Some("USDC"));
    }

    #[test]
    fn pool_refs_single_shorthand_and_list() {
        let single: WatchedToken = serde_json::from_str(
            r#"{"symbol":"A","mint":"M1","pool":"P1","quote":"USDC"}"#).unwrap();
        assert_eq!(single.pool_refs().len(), 1);
        assert_eq!(single.pool_refs()[0].pool, "P1");
        let multi: WatchedToken = serde_json::from_str(
            r#"{"symbol":"B","mint":"M2","pools":[{"pool":"P2","quote":"USDC"},{"pool":"P3","quote":"SOL"}]}"#).unwrap();
        assert_eq!(multi.pool_refs().len(), 2);
        let none: WatchedToken = serde_json::from_str(r#"{"symbol":"C","mint":"M3"}"#).unwrap();
        assert!(none.pool_refs().is_empty());
    }

    #[test]
    fn pool_refs_prefers_pools_when_both_present() {
        // Single-pool shorthand AND the pools list both set on the same entry: pools
        // wins outright (the shorthand is ignored, not merged).
        let both: WatchedToken = serde_json::from_str(
            r#"{"symbol":"D","mint":"M4","pool":"POLD","quote":"USDC","pools":[{"pool":"PNEW","quote":"SOL"}]}"#).unwrap();
        assert_eq!(both.pool_refs().len(), 1);
        assert_eq!(both.pool_refs()[0].pool, "PNEW");
        assert_eq!(both.pool_refs()[0].quote, "SOL");
    }

    #[test]
    fn load_warns_but_pools_wins_when_both_present() {
        // Same ambiguous shape, but exercised through `load()` — the call site the brief
        // wants the once-at-load warning attached to. `load()` must not error, and the
        // survivor's pool_refs() must still resolve to `pools` (not merge/concat).
        let path = write_tokens(&format!(
            r#"[{{"symbol":"D","mint":"{RAY}","pool":"POLD","quote":"USDC","pools":[{{"pool":"PNEW","quote":"SOL"}}]}}]"#
        ));
        let got = load(&path).unwrap();
        assert_eq!(got.len(), 1);
        let refs = got[0].pool_refs();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].pool, "PNEW");
        std::fs::remove_file(&path).ok();
    }
}
