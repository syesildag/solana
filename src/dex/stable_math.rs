/// Curve StableSwap invariant for 2-token pools.
///
/// Invariant: 4A(x+y) + D = 4AD + D³/(4xy)
/// where A = amplification coefficient, D = total-liquidity scalar.
///
/// All arithmetic is done in u128/i128. The D_P computation is kept stepwise
/// (D_P = D·D/(2x) then ·D/(2y)) so intermediates stay ≈ D in magnitude and
/// never overflow even for pools with >$1B TVL.

/// Compute invariant D given reserves x, y and amplification coefficient amp.
/// Uses Newton's method (≤255 iterations, converges in <10 for typical inputs).
pub fn compute_d(x: u64, y: u64, amp: u64) -> u64 {
    let s = x as u128 + y as u128;
    if s == 0 {
        return 0;
    }
    let ann = amp as u128 * 2; // A * N_COINS (N=2)
    let x128 = x as u128;
    let y128 = y as u128;
    let mut d = s;

    for _ in 0..255 {
        // D_P = D^3 / (4*x*y) computed stepwise to avoid materialising D^3:
        //   step 1: D * D / (2*x)
        //   step 2: result * D / (2*y)
        let d_p = d.saturating_mul(d) / (2 * x128).max(1);
        let d_p = d_p.saturating_mul(d) / (2 * y128).max(1);

        let d_prev = d;
        // Newton step: D = (ann*S + 2*D_P) * D / ((ann-1)*D + 3*D_P)
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

/// Compute new reserve_out (y) given updated reserve_in (new_x) and invariant D.
/// Used to find how much output token remains after a swap input is added.
fn compute_y(new_x: u64, d: u64, amp: u64) -> u64 {
    let ann = amp as i128 * 2;
    let d = d as i128;
    let new_x = new_x as i128;

    // Reduce 2-token invariant to quadratic in y:
    //   y_{n+1} = (y_n^2 + c) / (2*y_n + b - D)
    // where c = D^3 / (4*new_x*Ann),  b = new_x + D/Ann
    let c = {
        let step = d.saturating_mul(d) / (2 * new_x).max(1);
        step.saturating_mul(d) / (2 * ann).max(1)
    };
    let b = new_x + d / ann;

    let mut y = d;
    for _ in 0..255 {
        let y_prev = y;
        let numerator = y.saturating_mul(y) + c;
        // denominator = 2y + b - D; b < D for near-peg pools so use signed arithmetic
        let denominator = 2 * y + b - d;
        if denominator <= 0 {
            break;
        }
        y = numerator / denominator;
        if (y - y_prev).abs() <= 1 {
            break;
        }
    }

    y as u64
}

/// Compute exact swap output using the Curve StableSwap invariant.
///
/// Applies fee to amount_in before computing the invariant-preserving output.
/// Returns 0 for degenerate inputs (zero reserves, zero amount).
pub fn get_amount_out(
    amount_in: u64,
    reserve_in: u64,
    reserve_out: u64,
    amp: u64,
    fee_bps: u64,
) -> u64 {
    if amount_in == 0 || reserve_in == 0 || reserve_out == 0 {
        return 0;
    }
    let amount_in_after_fee =
        (amount_in as u128 * (10_000 - fee_bps as u128) / 10_000) as u64;
    let new_reserve_in = reserve_in.saturating_add(amount_in_after_fee);

    let d = compute_d(reserve_in, reserve_out, amp);
    let new_reserve_out = compute_y(new_reserve_in, d, amp);

    reserve_out.saturating_sub(new_reserve_out)
}

/// Approximate marginal exchange rate using a small probe (1/10_000 of reserve_in).
/// Used by the exchange graph to compute edge weights (-ln(rate)).
pub fn marginal_rate(reserve_in: u64, reserve_out: u64, amp: u64, fee_bps: u64) -> f64 {
    let probe = (reserve_in / 10_000).max(1);
    let out = get_amount_out(probe, reserve_in, reserve_out, amp, fee_bps);
    out as f64 / probe as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    // 50M USDC / 50M USDT pool (6-decimal tokens → 5e13 each)
    const X: u64 = 50_000_000_000_000;
    const Y: u64 = 50_000_000_000_000;
    const AMP: u64 = 100;

    #[test]
    fn compute_d_equal_reserves() {
        // For a balanced pool D ≈ x + y
        let d = compute_d(X, Y, AMP);
        let expected = X + Y;
        let tolerance = expected / 1_000; // 0.1%
        assert!(d.abs_diff(expected) <= tolerance, "D={d}, expected≈{expected}");
    }

    #[test]
    fn get_amount_out_near_peg() {
        // 1 USDC in → should get ≈ 1 USDT minus 0.05% fee
        let amount_in = 1_000_000u64;
        let out = get_amount_out(amount_in, X, Y, AMP, 5);
        assert!(out > 999_000 && out < 1_000_000, "out={out}");
    }

    #[test]
    fn get_amount_out_zero_in_returns_zero() {
        assert_eq!(get_amount_out(0, X, Y, AMP, 5), 0);
    }

    #[test]
    fn get_amount_out_zero_reserves_returns_zero() {
        assert_eq!(get_amount_out(1_000_000, 0, Y, AMP, 5), 0);
    }

    #[test]
    fn marginal_rate_near_one_for_equal_reserves() {
        let rate = marginal_rate(X, Y, AMP, 5);
        // ≈ 0.9995 (1:1 minus 0.05% fee)
        assert!(rate > 0.999 && rate < 1.001, "rate={rate}");
    }

    #[test]
    fn higher_amp_gives_better_rate_near_peg() {
        let rate_low  = marginal_rate(X, Y, 10, 5);
        let rate_high = marginal_rate(X, Y, 1000, 5);
        assert!(rate_high > rate_low, "higher amp should produce rate closer to 1:1");
    }

    #[test]
    fn round_trip_always_loses_money() {
        let mid = get_amount_out(1_000_000, X, Y, AMP, 5);
        let back = get_amount_out(mid, Y, X, AMP, 5);
        assert!(back < 1_000_000, "round-trip returned {back}, expected < 1_000_000");
    }
}
