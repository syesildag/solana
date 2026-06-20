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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchedToken {
    pub symbol: String,
    pub mint: String,
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
    fn malformed_file_errors() {
        let path = write_tokens("{ not an array }");
        assert!(load(&path).is_err());
        std::fs::remove_file(&path).ok();
    }
}
