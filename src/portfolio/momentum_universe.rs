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
/// metric/lookback/regime/rotate stay global.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_metric: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trail_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_pct: Option<f64>,
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
    /// Optional per-token parameter overrides (min_metric/trail_pct/max_run_pct);
    /// each falls back to the global config when absent. See `TokenParams`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<TokenParams>,
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
    fn token_without_params_serializes_without_the_key() {
        let w = WatchedToken {
            symbol: "AAA".into(),
            mint: "So11111111111111111111111111111111111111112".into(),
            name: None,
            equity: None,
            params: None,
        };
        let s = serde_json::to_string(&w).unwrap();
        assert!(!s.contains("params"), "no params key when None, got: {s}");
    }
}
