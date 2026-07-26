//! Stale-pool backfill poller.
//!
//! Free/shared gRPC feeds cap or throttle large account subscriptions, so
//! low-activity pools go seconds-to-minutes stale in the graph and their
//! cycles get (correctly) staleness-gated — phantom edges, no trades. This
//! poller backfills exactly those blind spots: every tick it finds the pools
//! whose `last_update_ns` exceeds a threshold, fetches their pricing accounts
//! in ONE `getMultipleAccounts` call (all accounts in a batch share a slot —
//! no intra-batch skew), applies the same parse paths as the gRPC callback,
//! stamps, refreshes the graph edge, and pokes the BF loop.
//!
//! It is a BACKFILL, not a feed replacement: fresh pools are never polled, so
//! RPC cost is a few calls/sec only while the feed is starving something.
//! Residual caveat: a polled pool is ~1 slot skewed vs gRPC-fresh pools.
//!
//! Not backfillable (skipped): Jupiter (has its own REST poller). PumpSwap IS
//! backfillable (two SPL vaults, same shape as Raydium AMM v4) — it enters the
//! arb registry under ENABLE_PUMPSWAP_TRADING, and a pump pool with no organic
//! swaps gets no gRPC vault writes, so without backfill its stamp ages without
//! bound and every cycle through it is staleness-gated forever (observed
//! 2026-07-26: PUMP/USDC 2uF4Xh61 at 335s, gating a persistent +28bps cycle).
//! Meteora DAMM IS backfillable: its two vault-LP token accounts are polled
//! and the cached virtual reserves scaled by the lp-balance ratio (same math
//! as the gRPC lp branch), provided the startup baseline was initialized.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::dex::{self, types::{monotonic_now_ns, DexKind, Pool}};
use crate::graph::exchange_graph::ExchangeGraph;

/// Hard cap on pools refreshed per tick — sized so one tick stays a single
/// getMultipleAccounts call (≤ 2 accounts/pool → ≤ 80 keys vs the RPC's 100
/// limit) while the sustained rate (40 / 0.3s ≈ 133 pools/s) comfortably beats
/// worst-case demand (all ~73 pools eligible every threshold window ≈ 90/s).
/// The original cap of 10 saturated on a starved feed: eligible pools queued
/// ~2.2s round-robin and oscillated ABOVE the 2s staleness gate, mass-gating
/// cycles while the median looked healthy (observed 2026-07-05 22:40).
const MAX_POOLS_PER_TICK: usize = 40;

pub struct PollTarget {
    pub pool: Arc<Pool>,
    pub accounts: Vec<Pubkey>,
    /// `last_update_ns` at selection time. If a live gRPC update lands while
    /// the poll is in flight, the stamp moves and the write is skipped —
    /// never overwrite newer stream data with older polled data.
    pub sel_stamp: u64,
}

/// The accounts that price this pool — mirrors the gRPC callback's paths:
/// CL-style pools (state_account present) price from pool state; CP pools
/// (Raydium AMM v4 / Saber) from their two SPL vaults. `None` = not
/// backfillable by a raw account fetch.
fn accounts_for(pool: &Pool) -> Option<Vec<Pubkey>> {
    match pool.dex {
        DexKind::Jupiter => None,
        // PumpSwap: plain CP with two SPL vaults (token-2022 sides keep amount
        // at offset 64, so the vault parse path applies unchanged).
        DexKind::PumpSwap => Some(vec![pool.vault_a, pool.vault_b]),
        // Meteora DAMM prices off vault-LP balances: poll both lp token accounts
        // and scale the cached virtual reserves by the balance ratio — identical
        // math to the gRPC callback's lp branch. Requires the startup-initialized
        // baseline (see apply); lp accounts are load-validated for DAMM.
        DexKind::MeteoraDamm => match (pool.extra.a_vault_lp, pool.extra.b_vault_lp) {
            (Some(a), Some(b)) => Some(vec![a, b]),
            _ => None,
        },
        _ => match pool.state_account {
            Some(s) => Some(vec![s]),
            None => match pool.dex {
                DexKind::RaydiumAmmV4 | DexKind::Saber => Some(vec![pool.vault_a, pool.vault_b]),
                _ => None, // CL kind missing its state account — nothing to poll
            },
        },
    }
}

/// Pools staler than `threshold_ms`, stalest first, capped at `max_pools`,
/// with the accounts to fetch for each. A never-stamped pool (0) counts as
/// `now_ns` old — the feed has never delivered it, the primary backfill case.
pub fn select_stale_targets(
    pools: &[Arc<Pool>],
    now_ns: u64,
    threshold_ms: u64,
    max_pools: usize,
) -> Vec<PollTarget> {
    let thr_ns = threshold_ms.saturating_mul(1_000_000);
    let mut stale: Vec<(u64, &Arc<Pool>)> = pools
        .iter()
        .filter_map(|p| {
            let stamp = p.last_update_ns.load(Ordering::Relaxed);
            let age = if stamp == 0 { now_ns } else { now_ns.saturating_sub(stamp) };
            (age > thr_ns).then_some((age, p))
        })
        .collect();
    stale.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    stale
        .into_iter()
        .filter_map(|(_, p)| {
            accounts_for(p).map(|accounts| PollTarget {
                pool: Arc::clone(p),
                accounts,
                sel_stamp: p.last_update_ns.load(Ordering::Relaxed),
            })
        })
        .take(max_pools)
        .collect()
}

/// Apply one polled account to its pool — same stores as the gRPC callback
/// (state → sqrt_price/fee, vault → reserve), then stamp + graph refresh.
/// Returns true if the pool was updated.
pub fn apply_polled_account(
    pool: &Arc<Pool>,
    key: &Pubkey,
    data: &[u8],
    graph: &ExchangeGraph,
) -> bool {
    let updated = if pool.state_account == Some(*key) {
        match dex::parse_cl_pool_state(data, pool) {
            Some((price, fee_bps)) => {
                pool.sqrt_price_x64.store(price.to_bits(), Ordering::Relaxed);
                if fee_bps > 0 {
                    pool.fee_bps.store(fee_bps, Ordering::Relaxed);
                }
                true
            }
            None => false,
        }
    } else if pool.extra.a_vault_lp == Some(*key) || pool.extra.b_vault_lp == Some(*key) {
        // Meteora DAMM: lp-token balance ratio scales the cached virtual reserve
        // (mirrors the gRPC callback's lp branch). Refuses to fabricate state
        // when the startup baseline is missing (old balance/reserve == 0).
        match dex::parse_spl_token_amount(data) {
            Some(new_bal) => {
                let is_a = pool.extra.a_vault_lp == Some(*key);
                let (old_bal, old_reserve) = if is_a {
                    (pool.a_lp_balance.load(Ordering::Relaxed), pool.reserve_a.load(Ordering::Relaxed))
                } else {
                    (pool.b_lp_balance.load(Ordering::Relaxed), pool.reserve_b.load(Ordering::Relaxed))
                };
                if old_bal > 0 && old_reserve > 0 {
                    let new_reserve =
                        ((old_reserve as f64) * (new_bal as f64 / old_bal as f64)) as u64;
                    if is_a {
                        pool.reserve_a.store(new_reserve, Ordering::Relaxed);
                        pool.a_lp_balance.store(new_bal, Ordering::Relaxed);
                    } else {
                        pool.reserve_b.store(new_reserve, Ordering::Relaxed);
                        pool.b_lp_balance.store(new_bal, Ordering::Relaxed);
                    }
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    } else if (*key == pool.vault_a || *key == pool.vault_b)
        && pool.dex != DexKind::MeteoraDamm
    {
        // Raw vault reserve (CP pools). DAMM vaults are excluded — their raw
        // balances are not the virtual reserves (same guard as the callback).
        match dex::parse_spl_token_amount(data) {
            Some(amount) => {
                if *key == pool.vault_a {
                    pool.reserve_a.store(amount, Ordering::Relaxed);
                } else {
                    pool.reserve_b.store(amount, Ordering::Relaxed);
                }
                true
            }
            None => false,
        }
    } else {
        false
    };
    if updated {
        pool.stamp_update();
        graph.update_pool(pool);
    }
    updated
}

/// Spawn the backfill task: every `interval_ms`, refresh up to
/// [`MAX_POOLS_PER_TICK`] stalest pools and poke the BF loop if anything
/// changed. RPC failures are logged and skipped — the staleness gate remains
/// the safety net for anything the poller can't keep fresh.
pub fn spawn_backfill_poller(
    pools: Vec<Arc<Pool>>,
    graph: Arc<ExchangeGraph>,
    rpc: Arc<RpcClient>,
    update_tx: watch::Sender<u64>,
    interval_ms: u64,
    threshold_ms: u64,
) {
    let watchable = pools
        .iter()
        .filter(|p| accounts_for(p).is_some())
        .count();
    info!(
        "Stale-pool backfill poller started (interval={interval_ms}ms threshold={threshold_ms}ms, \
         {watchable}/{} pools backfillable)",
        pools.len()
    );
    tokio::spawn(async move {
        let interval = std::time::Duration::from_millis(interval_ms.max(100));
        loop {
            tokio::time::sleep(interval).await;
            let targets =
                select_stale_targets(&pools, monotonic_now_ns(), threshold_ms, MAX_POOLS_PER_TICK);
            if targets.is_empty() {
                continue;
            }
            let keys: Vec<Pubkey> =
                targets.iter().flat_map(|t| t.accounts.iter().copied()).collect();
            let accounts = match rpc.get_multiple_accounts(&keys).await {
                Ok(a) => a,
                Err(e) => {
                    warn!("backfill poll failed ({} accounts): {}", keys.len(),
                        crate::dex::types::redact_secrets(&e.to_string()));
                    continue;
                }
            };
            let mut idx = 0;
            let mut refreshed = 0usize;
            for t in &targets {
                // A live stream update won the race for this pool — its data is
                // newer than our fetch; leave it alone.
                let live_won = t.pool.last_update_ns.load(Ordering::Relaxed) != t.sel_stamp;
                for key in &t.accounts {
                    let acc = accounts.get(idx).and_then(|o| o.as_ref());
                    idx += 1;
                    if live_won {
                        continue;
                    }
                    if let Some(acc) = acc {
                        if apply_polled_account(&t.pool, key, &acc.data, &graph) {
                            refreshed += 1;
                        }
                    }
                }
            }
            if refreshed > 0 {
                debug!(
                    "backfill: refreshed {refreshed} accounts across {} stale pools",
                    targets.len()
                );
                update_tx.send_modify(|v| *v = v.wrapping_add(1));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::PoolExtra;
    use std::sync::atomic::{AtomicI32, AtomicU64};

    fn pool(dex: DexKind, state: Option<Pubkey>) -> Arc<Pool> {
        Arc::new(Pool {
            id: Pubkey::new_unique(),
            dex,
            token_a: Pubkey::new_unique(),
            token_b: Pubkey::new_unique(),
            vault_a: Pubkey::new_unique(),
            vault_b: Pubkey::new_unique(),
            reserve_a: AtomicU64::new(0),
            reserve_b: AtomicU64::new(0),
            fee_bps: AtomicU64::new(25),
            sqrt_price_x64: AtomicU64::new(0),
            active_bin_id: AtomicI32::new(0),
            tick_current_index: AtomicI32::new(0),
            state_account: state,
            stable: false,
            damm_virtual_price: AtomicU64::new(0),
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            extra: PoolExtra::default(),
            clmm_tick_array_bitmap: std::array::from_fn(|_| AtomicU64::new(0)),
            clmm_observation_key: std::array::from_fn(|_| AtomicU64::new(0)),
            dlmm_token_a_is_x: AtomicU64::new(0),
            last_update_ns: AtomicU64::new(0),
        })
    }

    /// Minimal SPL token account: amount is a u64 LE at byte offset 64.
    fn spl_token_data(amount: u64) -> Vec<u8> {
        let mut d = vec![0u8; 165];
        d[64..72].copy_from_slice(&amount.to_le_bytes());
        d
    }

    #[test]
    fn selects_stalest_first_caps_and_skips_unbackfillable() {
        let now = 100_000_000_000u64; // 100s
        let stamp = |p: &Arc<Pool>, age_ns: u64| {
            p.last_update_ns.store(now - age_ns, Ordering::Relaxed)
        };
        let cp_stale = pool(DexKind::RaydiumAmmV4, None);          // 50s old → 2 vault accounts
        stamp(&cp_stale, 50_000_000_000);
        let cl_staler = pool(DexKind::OrcaWhirlpool, Some(Pubkey::new_unique())); // 80s → 1 state account
        stamp(&cl_staler, 80_000_000_000);
        let cp_fresh = pool(DexKind::RaydiumAmmV4, None);          // 1s → not stale
        stamp(&cp_fresh, 1_000_000_000);
        let jup = Pool::new_jupiter(Pubkey::new_unique(), Pubkey::new_unique()); // stale (never) but excluded
        let damm = pool(DexKind::MeteoraDamm, None);               // stale but excluded
        stamp(&damm, 90_000_000_000);
        let pools = vec![
            Arc::clone(&cp_stale), Arc::clone(&cl_staler), Arc::clone(&cp_fresh), jup, damm,
        ];

        let t = select_stale_targets(&pools, now, 3_000, 10);
        assert_eq!(t.len(), 2, "jupiter/damm/fresh skipped");
        assert_eq!(t[0].pool.id, cl_staler.id, "stalest first");
        assert_eq!(t[0].accounts, vec![cl_staler.state_account.unwrap()]);
        assert_eq!(t[1].pool.id, cp_stale.id);
        assert_eq!(t[1].accounts, vec![cp_stale.vault_a, cp_stale.vault_b]);

        let capped = select_stale_targets(&pools, now, 3_000, 1);
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].pool.id, cl_staler.id, "cap keeps the stalest");
    }

    #[test]
    fn pumpswap_pools_poll_their_vaults() {
        let p = pool(DexKind::PumpSwap, None); // stamp 0 → stale
        let graph = ExchangeGraph::new();
        let t = select_stale_targets(&[Arc::clone(&p)], 10_000_000_000, 800, 10);
        assert_eq!(t.len(), 1, "PumpSwap must be backfillable via its vaults");
        assert_eq!(t[0].accounts, vec![p.vault_a, p.vault_b]);
        assert!(apply_polled_account(&p, &p.vault_a.clone(), &spl_token_data(999), &graph));
        assert_eq!(p.reserve_a.load(Ordering::Relaxed), 999);
        assert!(p.last_update_ns.load(Ordering::Relaxed) >= 1, "stamped");
    }

    #[test]
    fn never_stamped_pool_is_primary_backfill_case() {
        let p = pool(DexKind::RaydiumAmmV4, None); // stamp 0 = feed never delivered it
        let t = select_stale_targets(&[Arc::clone(&p)], 10_000_000_000, 3_000, 10);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].sel_stamp, 0);
    }

    #[test]
    fn apply_vault_data_updates_reserve_stamp_and_returns_true() {
        let p = pool(DexKind::RaydiumAmmV4, None);
        let graph = ExchangeGraph::new();
        assert!(apply_polled_account(&p, &p.vault_a.clone(), &spl_token_data(777), &graph));
        assert_eq!(p.reserve_a.load(Ordering::Relaxed), 777);
        assert!(p.last_update_ns.load(Ordering::Relaxed) >= 1, "stamped");
        assert!(apply_polled_account(&p, &p.vault_b.clone(), &spl_token_data(555), &graph));
        assert_eq!(p.reserve_b.load(Ordering::Relaxed), 555);
    }

    fn damm_pool() -> Arc<Pool> {
        let p = pool(DexKind::MeteoraDamm, None);
        // Arc::try_unwrap to inject the lp accounts into extra, then re-wrap.
        let mut inner = std::sync::Arc::try_unwrap(p).ok().unwrap();
        inner.extra.a_vault_lp = Some(Pubkey::new_unique());
        inner.extra.b_vault_lp = Some(Pubkey::new_unique());
        Arc::new(inner)
    }

    #[test]
    fn damm_pools_poll_their_lp_accounts() {
        let p = damm_pool(); // stamp 0 → stale
        let t = select_stale_targets(&[Arc::clone(&p)], 10_000_000_000, 800, 10);
        assert_eq!(t.len(), 1, "DAMM must be backfillable via lp accounts");
        assert_eq!(
            t[0].accounts,
            vec![p.extra.a_vault_lp.unwrap(), p.extra.b_vault_lp.unwrap()]
        );
    }

    #[test]
    fn apply_damm_lp_ratio_scales_reserves() {
        let p = damm_pool();
        let graph = ExchangeGraph::new();
        // Startup-initialized baseline: lp 1_000 ↔ virtual reserve 500_000.
        p.a_lp_balance.store(1_000, Ordering::Relaxed);
        p.reserve_a.store(500_000, Ordering::Relaxed);
        let lp_a = p.extra.a_vault_lp.unwrap();
        // LP balance grew 10% → reserve scales 10% (same math as the gRPC lp branch).
        assert!(apply_polled_account(&p, &lp_a, &spl_token_data(1_100), &graph));
        assert_eq!(p.reserve_a.load(Ordering::Relaxed), 550_000);
        assert_eq!(p.a_lp_balance.load(Ordering::Relaxed), 1_100);
        assert!(p.last_update_ns.load(Ordering::Relaxed) >= 1, "stamped");
    }

    #[test]
    fn apply_damm_without_baseline_refuses_to_fabricate() {
        let p = damm_pool(); // a_lp_balance / reserve_a still 0 — no startup init
        let graph = ExchangeGraph::new();
        let lp_a = p.extra.a_vault_lp.unwrap();
        assert!(!apply_polled_account(&p, &lp_a, &spl_token_data(1_100), &graph),
            "ratio update needs a valid baseline");
        assert_eq!(p.last_update_ns.load(Ordering::Relaxed), 0, "must not stamp");
    }

    #[test]
    fn apply_rejects_garbage_and_unknown_accounts() {
        let p = pool(DexKind::RaydiumAmmV4, None);
        let graph = ExchangeGraph::new();
        assert!(!apply_polled_account(&p, &p.vault_a.clone(), &[0u8; 10], &graph),
            "short data must not parse");
        assert!(!apply_polled_account(&p, &Pubkey::new_unique(), &spl_token_data(9), &graph),
            "unknown account must be ignored");
        assert_eq!(p.last_update_ns.load(Ordering::Relaxed), 0, "no stamp on rejected data");
    }
}
