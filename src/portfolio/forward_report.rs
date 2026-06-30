//! Read-only forward-test reconciliation for the paper momentum trader.
//! Parses momentum_actions.jsonl, computes realized metrics, replays the live
//! config over the same forward window for the prediction, and reconciles.
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

pub const PERIODS_PER_YEAR: f64 = 365.0 * 86_400.0 / 184.0;

#[derive(Debug, Clone)]
pub struct ClosedTrip { pub symbol: String, pub entry_ts: u64, pub exit_ts: u64, pub usdc_in: f64, pub usdc_out: f64, pub reason: String, pub dry_run: bool }
#[derive(Debug, Clone)]
pub struct OpenPosition { pub symbol: String, pub entry_ts: u64, pub usdc_in: f64, pub entry_price_usd: f64 }
#[derive(Debug, Clone)]
pub struct ConfigPoint { pub ts: u64, pub metric: String, pub min_score: f64 }
#[derive(Debug, Clone, Default)]
pub struct ParsedLog { pub closed: Vec<ClosedTrip>, pub open: Vec<OpenPosition>, pub real_filtered: usize, pub config_points: Vec<ConfigPoint>, pub first_ts: Option<u64>, pub last_ts: Option<u64> }

fn rfc3339(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp() as u64)
}

pub fn parse_actions(path: &Path, since: Option<u64>, paper_only: bool) -> Result<ParsedLog> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    // symbol -> (entry_ts, usdc_in, entry_price)
    let mut open: HashMap<String, (u64, f64, f64)> = HashMap::new();
    let mut out = ParsedLog::default();
    for line in text.lines() {
        if line.trim().is_empty() { continue; }
        let v: Value = match serde_json::from_str(line) { Ok(v) => v, Err(_) => continue };
        let ts = match v.get("ts").and_then(|x| x.as_str()).and_then(rfc3339) { Some(t) => t, None => continue };
        out.first_ts = Some(out.first_ts.map_or(ts, |f| f.min(ts)));
        out.last_ts = Some(out.last_ts.map_or(ts, |f| f.max(ts)));
        if since.is_some_and(|s| ts < s) { continue; }
        let dry = v.get("dry_run").and_then(|x| x.as_bool()).unwrap_or(true);
        let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("");
        let f = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let sym = v.get("symbol").and_then(|x| x.as_str()).unwrap_or("").to_string();
        match kind {
            "RankSnapshot" => out.config_points.push(ConfigPoint {
                ts,
                metric: v.get("metric").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                min_score: f("min_score"),
            }),
            "Entered" | "Rotated" => {
                if paper_only && !dry { out.real_filtered += 1; continue; }
                open.insert(sym, (ts, f("usdc_in"), f("entry_price_usd")));
            }
            "Exited" => {
                if paper_only && !dry { out.real_filtered += 1; continue; }
                if let Some((entry_ts, usdc_in, _)) = open.remove(&sym) {
                    out.closed.push(ClosedTrip {
                        symbol: sym, entry_ts, exit_ts: ts, usdc_in, usdc_out: f("usdc_out"),
                        reason: v.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string(), dry_run: dry,
                    });
                }
            }
            _ => {}
        }
    }
    for (symbol, (entry_ts, usdc_in, entry_price_usd)) in open {
        out.open.push(OpenPosition { symbol, entry_ts, usdc_in, entry_price_usd });
    }
    out.closed.sort_by_key(|t| t.exit_ts);
    out.open.sort_by_key(|o| o.entry_ts);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_log(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for l in lines { writeln!(f, "{l}").unwrap(); }
        f
    }

    #[test]
    fn parses_one_closed_round_trip() {
        let f = tmp_log(&[
            r#"{"ts":"2025-06-21T08:00:00Z","kind":"Entered","symbol":"BP","mint":"m","usdc_in":100.0,"token_amount":1.0,"entry_price_usd":100.0,"cost_bps":25,"sig":"s","dry_run":true}"#,
            r#"{"ts":"2025-06-21T12:00:00Z","kind":"Exited","symbol":"BP","mint":"m","usdc_out":105.0,"exit_price_usd":105.0,"peak_price_usd":106.0,"pnl_pct":5.0,"reason":"trailing stop","sig":"s","dry_run":true}"#,
        ]);
        let p = parse_actions(f.path(), None, true).unwrap();
        assert_eq!(p.closed.len(), 1);
        assert_eq!(p.open.len(), 0);
        let t = &p.closed[0];
        assert_eq!(t.symbol, "BP");
        assert!((t.usdc_out - 105.0).abs() < 1e-9);
        assert_eq!(t.entry_ts, 1750492800); // 2025-06-21T08:00:00Z
    }

    #[test]
    fn unmatched_entry_is_open_not_closed() {
        let f = tmp_log(&[
            r#"{"ts":"2025-06-21T08:00:00Z","kind":"Entered","symbol":"MET","usdc_in":100.0,"entry_price_usd":0.16,"dry_run":true}"#,
        ]);
        let p = parse_actions(f.path(), None, true).unwrap();
        assert_eq!(p.closed.len(), 0);
        assert_eq!(p.open.len(), 1);
        assert_eq!(p.open[0].symbol, "MET");
    }

    #[test]
    fn paper_only_filters_real_trades() {
        let f = tmp_log(&[
            r#"{"ts":"2025-06-21T08:00:00Z","kind":"Entered","symbol":"BP","usdc_in":100.0,"entry_price_usd":1.0,"dry_run":false}"#,
            r#"{"ts":"2025-06-21T12:00:00Z","kind":"Exited","symbol":"BP","usdc_out":110.0,"reason":"x","dry_run":false}"#,
        ]);
        let p = parse_actions(f.path(), None, true).unwrap();
        assert_eq!(p.closed.len(), 0);
        assert_eq!(p.real_filtered, 2);
    }

    #[test]
    fn since_excludes_earlier_events() {
        let f = tmp_log(&[
            r#"{"ts":"2025-06-20T08:00:00Z","kind":"Entered","symbol":"BP","usdc_in":100.0,"entry_price_usd":1.0,"dry_run":true}"#,
            r#"{"ts":"2025-06-20T12:00:00Z","kind":"Exited","symbol":"BP","usdc_out":110.0,"reason":"x","dry_run":true}"#,
        ]);
        let since = chrono::DateTime::parse_from_rfc3339("2025-06-21T00:00:00Z").unwrap().timestamp() as u64;
        let p = parse_actions(f.path(), Some(since), true).unwrap();
        assert_eq!(p.closed.len(), 0);
    }
}
