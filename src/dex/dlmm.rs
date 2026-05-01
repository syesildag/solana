use crate::dex::types;

/// Parse a Meteora DLMM LbPair state account and return (price_as_f64, fee_bps).
///
/// The price returned is "token_b raw per token_a raw" — the same convention used by
/// Raydium CLMM and Orca Whirlpool so that exchange_graph::update_pool can reuse the
/// same sqrt_price_x64 hot path without modification.
///
/// Formula: raw_price_y_per_x = (1 + binStep/10_000)^active_id
///
/// Where active_id (i32 at offset 76) encodes the current active bin, and the result
/// is the raw (lamport) price — token_y raw per token_x raw.  No decimal scaling is
/// needed because the DLMM on-chain price is already expressed in raw (lamport) units.
///
/// Direction is resolved by reading token_x_mint (offset 88, 32 bytes) and comparing
/// to pool.token_a.  If they match, token_b == token_y, and raw_price_y_per_x is the
/// answer.  Otherwise the pool is stored reversed, and we return 1 / raw_price.
///
/// We return fee_bps = 0 to preserve the value loaded at startup (pool.fee_bps is set
/// from pools.json and remains constant for the lifetime of the pool).
pub fn parse_state(data: &[u8], pool: &types::Pool) -> Option<(f64, u64)> {
    if data.len() < 120 {
        return None;
    }

    let active_id = i32::from_le_bytes(data[76..80].try_into().ok()?);
    let bin_step   = pool.extra.dlmm_bin_step? as f64;

    let raw_price_y_per_x = (1.0_f64 + bin_step / 10_000.0).powi(active_id);

    if !raw_price_y_per_x.is_finite() || raw_price_y_per_x <= 0.0 {
        return None;
    }

    // token_x_mint is at offset 88 in the LbPair account (32 bytes).
    let token_x_in_state = &data[88..120];
    let is_a_token_x     = token_x_in_state == pool.token_a.as_ref();

    let price = if is_a_token_x {
        raw_price_y_per_x          // token_b == token_y
    } else {
        1.0 / raw_price_y_per_x    // pool stored reversed; token_b == token_x
    };

    if !price.is_finite() || price <= 0.0 {
        return None;
    }

    Some((price, 0))
}
