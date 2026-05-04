/// Curve StableSwap invariant for 2-token pools.
///
/// Invariant: 4A(x+y) + D = 4AD + D³/(4xy)
/// where A = amplification coefficient, D = total-liquidity scalar.
///
/// For LST/SOL pairs (e.g. SOL/mSOL) the pool operates in "token_a-equivalent"
/// space: reserve_b is scaled by `price_scale / PRICE_SCALE` before the invariant
/// is applied, and outputs are un-scaled afterward.  For true stablecoin pairs
/// (USDC/USDT) `price_scale == PRICE_SCALE` so scaling is a no-op.
///
/// All arithmetic uses u128/i128; the D_P computation is done stepwise so that
/// intermediates stay ≈ D in magnitude and never overflow for pools up to $1T TVL.

/// Fixed-point precision for `price_scale`.  1.375 SOL/mSOL is stored as 1_375_000_000.
pub const PRICE_SCALE: u64 = 1_000_000_000;

/// Compute invariant D given reserves x, y and amplification coefficient amp.
/// Operates in u128 internally; caller passes scaled reserves (both in token_a-equiv units).
fn compute_d(x: u64, y: u64, amp: u64) -> u64 {
    let s = x as u128 + y as u128;
    if s == 0 {
        return 0;
    }
    let ann = amp as u128 * 2;
    let x128 = x as u128;
    let y128 = y as u128;
    let mut d = s;

    for _ in 0..255 {
        let d_p = d.saturating_mul(d) / (2 * x128).max(1);
        let d_p = d_p.saturating_mul(d) / (2 * y128).max(1);
        let d_prev = d;
        let numerator = (ann.saturating_mul(s) + d_p.saturating_mul(2)).saturating_mul(d);
        let denominator = (ann - 1).saturating_mul(d) + d_p.saturating_mul(3);
        if denominator == 0 {
            break;
        }
        d = numerator / denominator;
        if d.abs_diff(d_prev) <= 1 {
            break;
        }
    }
    d as u64
}

/// Given the new value of one reserve and the invariant D, compute the other reserve.
/// The Curve invariant is symmetric in its two reserve terms, so this finds either
/// "new_y given new_x" or "new_x given new_y" depending on which you pass as `given`.
fn compute_other(given: u64, d: u64, amp: u64) -> u64 {
    let ann = amp as i128 * 2;
    let d = d as i128;
    let given = given as i128;

    // Reduce invariant to quadratic in the unknown (call it `u`):
    //   u_{n+1} = (u_n^2 + c) / (2·u_n + b - D)
    // where  c = D³/(4·given·Ann),  b = given + D/Ann
    let c = {
        let step = d.saturating_mul(d) / (2 * given).max(1);
        step.saturating_mul(d) / (2 * ann).max(1)
    };
    let b = given + d / ann;

    let mut u = d;
    for _ in 0..255 {
        let u_prev = u;
        let numerator = u.saturating_mul(u) + c;
        let denominator = 2 * u + b - d;
        if denominator <= 0 {
            break;
        }
        u = numerator / denominator;
        if (u - u_prev).abs() <= 1 {
            break;
        }
    }
    u as u64
}

/// Compute exact swap output using the Curve StableSwap invariant with optional
/// virtual-price scaling for LST/SOL pools.
///
/// `price_scale` encodes how much token_a one unit of token_b is worth:
///   - PRICE_SCALE (1e9) = 1:1 peg (USDC/USDT)  — no scaling
///   - 1_375_000_000     = 1.375× (SOL/mSOL)    — scale reserve_b up before invariant
///
/// `a_to_b` specifies swap direction: true = token_a → token_b, false = b → a.
pub fn get_amount_out(
    amount_in: u64,
    reserve_a: u64,
    reserve_b: u64,
    amp: u64,
    fee_bps: u64,
    price_scale: u64,
    a_to_b: bool,
) -> u64 {
    if amount_in == 0 || reserve_a == 0 || reserve_b == 0 {
        return 0;
    }

    let scale = PRICE_SCALE as u128;
    let vpr   = price_scale as u128;

    // Scale reserve_b into token_a-equivalent space.
    // Intermediate product uses u128 to avoid overflow before the division.
    let x: u64 = reserve_a;
    let y_eff: u64 = ((reserve_b as u128).saturating_mul(vpr) / scale)
        .min(u64::MAX as u128) as u64;

    let d = compute_d(x, y_eff, amp);

    let amount_in_after_fee =
        (amount_in as u128 * (10_000 - fee_bps as u128) / 10_000) as u64;

    if a_to_b {
        // token_a → token_b: add to x side, compute new y_eff, un-scale
        let new_x = x.saturating_add(amount_in_after_fee);
        let new_y_eff = compute_other(new_x, d, amp);
        let delta_y_eff = y_eff.saturating_sub(new_y_eff) as u128;
        // Un-scale: raw mSOL out = delta_y_eff × SCALE / vpr
        (delta_y_eff * scale / vpr.max(1))
            .min(u64::MAX as u128) as u64
    } else {
        // token_b → token_a: scale the input, add to y_eff, compute new x
        let amount_in_eff = ((amount_in_after_fee as u128).saturating_mul(vpr) / scale) as u64;
        let new_y_eff = y_eff.saturating_add(amount_in_eff);
        let new_x = compute_other(new_y_eff, d, amp);
        x.saturating_sub(new_x)
    }
}

/// Approximate marginal exchange rate using a small probe (1/10_000 of the input reserve).
///
/// `reserve_a` and `reserve_b` are always in canonical pool order (token_a, token_b).
/// `a_to_b` specifies which direction to probe.
#[cfg_attr(not(test), allow(dead_code))]
pub fn marginal_rate(
    reserve_a: u64,
    reserve_b: u64,
    amp: u64,
    fee_bps: u64,
    price_scale: u64,
    a_to_b: bool,
) -> f64 {
    let probe_in = if a_to_b {
        (reserve_a / 10_000).max(1)
    } else {
        (reserve_b / 10_000).max(1)
    };
    let out = get_amount_out(probe_in, reserve_a, reserve_b, amp, fee_bps, price_scale, a_to_b);
    out as f64 / probe_in as f64
}

/// Swap quote for a Meteora DAMM stable pool.
///
/// Per the official Meteora DAMM SDK (`calculateSwapQuote`), the StableSwap invariant
/// operates at the raw token reserve level (`ra`/`rb`), not at the vault-LP level.
/// `ra = vaultAReserve * poolVaultALp / vaultALpSupply` — already computed by the
/// pool state parser and stored in `pool.reserve_a`/`reserve_b`.
///
/// Parameters:
///   `ra` / `rb` — raw token reserves.
///   `price_scale` — base_virtual_price in Q9 (depeg/oracle price, e.g. mSOL/SOL).
pub fn get_amount_out_damm(
    amount_in: u64,
    ra: u64, rb: u64,
    amp: u64,
    fee_bps: u64,
    price_scale: u64,
    a_to_b: bool,
) -> u64 {
    get_amount_out(amount_in, ra, rb, amp, fee_bps, price_scale, a_to_b)
}

/// Marginal rate for a Meteora DAMM stable pool.
pub fn marginal_rate_damm(
    ra: u64, rb: u64,
    amp: u64,
    fee_bps: u64,
    price_scale: u64,
    a_to_b: bool,
) -> f64 {
    marginal_rate(ra, rb, amp, fee_bps, price_scale, a_to_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 50M USDC / 50M USDT pool (6-decimal tokens → 5e13 each)
    const X: u64 = 50_000_000_000_000;
    const Y: u64 = 50_000_000_000_000;
    const AMP: u64 = 100;

    // ── 1:1 peg (USDC/USDT) ─────────────────────────────────────────────────────

    #[test]
    fn compute_d_equal_reserves() {
        let d = compute_d(X, Y, AMP);
        let expected = X + Y;
        let tolerance = expected / 1_000;
        assert!(d.abs_diff(expected) <= tolerance, "D={d}, expected≈{expected}");
    }

    #[test]
    fn get_amount_out_near_peg() {
        let out = get_amount_out(1_000_000, X, Y, AMP, 5, PRICE_SCALE, true);
        assert!(out > 999_000 && out < 1_000_000, "out={out}");
    }

    #[test]
    fn get_amount_out_zero_in_returns_zero() {
        assert_eq!(get_amount_out(0, X, Y, AMP, 5, PRICE_SCALE, true), 0);
    }

    #[test]
    fn get_amount_out_zero_reserves_returns_zero() {
        assert_eq!(get_amount_out(1_000_000, 0, Y, AMP, 5, PRICE_SCALE, true), 0);
    }

    #[test]
    fn marginal_rate_near_one_for_equal_reserves() {
        let rate = marginal_rate(X, Y, AMP, 5, PRICE_SCALE, true);
        assert!(rate > 0.999 && rate < 1.001, "rate={rate}");
    }

    #[test]
    fn higher_amp_gives_better_rate_near_peg() {
        let rate_low  = marginal_rate(X, Y, 10, 5, PRICE_SCALE, true);
        let rate_high = marginal_rate(X, Y, 1000, 5, PRICE_SCALE, true);
        assert!(rate_high > rate_low);
    }

    #[test]
    fn round_trip_always_loses_money() {
        let mid  = get_amount_out(1_000_000, X, Y, AMP, 5, PRICE_SCALE, true);
        let back = get_amount_out(mid, X, Y, AMP, 5, PRICE_SCALE, false);
        assert!(back < 1_000_000, "round-trip returned {back}");
    }

    // ── Virtual price scaling (SOL/mSOL at 1.375 SOL/mSOL) ──────────────────────

    // Pool: 1_000 SOL (reserve_a) / 727.27 mSOL (reserve_b)
    // At 1 mSOL = 1.375 SOL: 727.27 mSOL × 1.375 ≈ 1_000 SOL → value-balanced pool
    const SOL_RESERVES:  u64 = 1_000_000_000_000; // 1000 SOL in lamports
    const MSOL_RESERVES: u64 =   727_272_727_272; // ≈727 mSOL in lamports
    const MSOL_VPR:      u64 = 1_375_000_000;     // 1.375 × PRICE_SCALE

    #[test]
    fn sol_to_msol_rate_with_virtual_price() {
        // 1 SOL in → expect ≈ 0.727 mSOL out (= 1 / 1.375 SOL/mSOL, minus fee)
        let out = get_amount_out(
            1_000_000_000, SOL_RESERVES, MSOL_RESERVES, AMP, 25, MSOL_VPR, true,
        );
        // 1 SOL = 10^9 lamports → expect 700M–730M mSOL lamports
        assert!(out > 700_000_000 && out < 730_000_000,
            "1 SOL → mSOL: expected ≈0.727 mSOL, got {} lamports", out);
    }

    #[test]
    fn msol_to_sol_rate_with_virtual_price() {
        // 1 mSOL in → expect ≈ 1.375 SOL out (minus fee)
        let out = get_amount_out(
            1_000_000_000, SOL_RESERVES, MSOL_RESERVES, AMP, 25, MSOL_VPR, false,
        );
        // 1 mSOL = 10^9 lamports → expect 1.35–1.40 SOL = 1_350M–1_400M lamports
        assert!(out > 1_350_000_000 && out < 1_400_000_000,
            "1 mSOL → SOL: expected ≈1.375 SOL, got {} lamports", out);
    }

    #[test]
    fn virtual_price_round_trip_loses_money() {
        let mid  = get_amount_out(1_000_000_000, SOL_RESERVES, MSOL_RESERVES, AMP, 25, MSOL_VPR, true);
        let back = get_amount_out(mid, SOL_RESERVES, MSOL_RESERVES, AMP, 25, MSOL_VPR, false);
        assert!(back < 1_000_000_000, "round-trip returned {back}, expected < 1e9");
    }

    #[test]
    fn marginal_rate_sol_to_msol_with_virtual_price() {
        let rate = marginal_rate(SOL_RESERVES, MSOL_RESERVES, AMP, 0, MSOL_VPR, true);
        // Expect ≈ 1/1.375 ≈ 0.727
        assert!(rate > 0.70 && rate < 0.76, "SOL→mSOL rate={rate:.4}, expected ≈0.727");
    }

    #[test]
    fn marginal_rate_msol_to_sol_with_virtual_price() {
        let rate = marginal_rate(SOL_RESERVES, MSOL_RESERVES, AMP, 0, MSOL_VPR, false);
        // Expect ≈ 1.375
        assert!(rate > 1.34 && rate < 1.41, "mSOL→SOL rate={rate:.4}, expected ≈1.375");
    }

    #[test]
    fn price_scale_equal_to_price_scale_is_identity() {
        // price_scale == PRICE_SCALE should give same results as old 1:1 behavior
        let out_scaled   = get_amount_out(1_000_000, X, Y, AMP, 5, PRICE_SCALE, true);
        let out_unscaled = get_amount_out(1_000_000, X, Y, AMP, 5, PRICE_SCALE, true);
        assert_eq!(out_scaled, out_unscaled);
    }
}
