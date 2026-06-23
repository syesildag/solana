//! Kamino `klend` integration for the on-chain pairs trader (Phase 2b).
//!
//! Hand-rolled Anchor instructions (no klend Rust crate, to avoid a conflicting
//! `solana-sdk` pin), mirroring the MarginFi flash-loan integration in
//! `src/flash_loan/mod.rs`. This file is the ONLY module that touches the klend
//! program.
//!
//! **Status (Phase 2b.1):** program id, Anchor discriminator, obligation PDA, and
//! the market/reserve types are implemented + unit-tested here. The instruction
//! builders (2b.2) and `Reserve` account parsing / health read need the live klend
//! IDL and devnet verification before use — see the stubs below. Do NOT fabricate
//! account orderings; derive them from the IDL (`anchor idl fetch`/the klend repo).

use std::collections::HashMap;

use solana_sdk::pubkey::Pubkey;

/// Kamino Lending (`klend`) program id — mainnet.
pub const KLEND_PROGRAM_ID: &str = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD";
/// Staging deployment on mainnet (for dry runs against the staging market).
pub const KLEND_STAGING_PROGRAM_ID: &str = "SLendK7ySfcEzyaFqy93gDnD3RtrpXJcnRwb6zFHJSh";

/// klend instruction names (Anchor `global:<name>` discriminators). Account orderings
/// for each are defined by the IDL and wired in Task 2b.2.
pub mod ix_name {
    pub const INIT_OBLIGATION: &str = "init_obligation";
    pub const REFRESH_RESERVE: &str = "refresh_reserve";
    pub const REFRESH_OBLIGATION: &str = "refresh_obligation";
    pub const DEPOSIT: &str = "deposit_reserve_liquidity_and_obligation_collateral";
    pub const WITHDRAW: &str = "withdraw_obligation_collateral_and_redeem_reserve_collateral";
    pub const BORROW: &str = "borrow_obligation_liquidity";
    pub const REPAY: &str = "repay_obligation_liquidity";
}

/// Anchor instruction discriminator: `sha256("global:<name>")[..8]`. Same scheme the
/// MarginFi integration uses; klend is also an Anchor program.
pub fn anchor_discriminator(name: &str) -> [u8; 8] {
    use solana_sdk::hash::hash;
    hash(format!("global:{name}").as_bytes()).to_bytes()[..8]
        .try_into()
        .expect("sha256 is always 32 bytes")
}

pub fn program_id() -> Pubkey {
    KLEND_PROGRAM_ID.parse().expect("valid klend program id")
}

/// Derive a user's obligation PDA for a given lending market.
///
/// klend obligation seeds: `[&[tag], &[id], owner, lending_market, seed1, seed2]`.
/// A vanilla user obligation uses `tag = 0`, `id = 0`, and `seed1 = seed2 =
/// Pubkey::default()`. (Verify against the live IDL/SDK before signing anything —
/// markets with non-default obligation configs use different tag/id/seed values.)
pub fn obligation_pda(owner: &Pubkey, lending_market: &Pubkey, program_id: &Pubkey) -> Pubkey {
    let default = Pubkey::default();
    Pubkey::find_program_address(
        &[
            &[0u8],            // tag
            &[0u8],            // id
            owner.as_ref(),
            lending_market.as_ref(),
            default.as_ref(),  // seed1
            default.as_ref(),  // seed2
        ],
        program_id,
    )
    .0
}

/// One borrowable/collateral reserve in a klend market, with the fields the pairs
/// trader's risk layer needs. Parsed from the on-chain `Reserve` account (offsets
/// from the IDL) in `load_market` — Task 2b.1 cont. / 2b.2.
#[derive(Debug, Clone)]
pub struct ReserveInfo {
    pub reserve: Pubkey,
    pub liquidity_mint: Pubkey,
    /// Liquidation threshold (0–1) — used by `sim::estimate_health_factor`.
    pub liq_threshold: f64,
    /// Current borrow APR/APY in percent — gated against `PAIRS_MAX_BORROW_APY_PCT`.
    pub borrow_apy_pct: f64,
    /// Available liquidity to borrow, in token units.
    pub available_liquidity: f64,
}

/// A loaded klend market: the program, the market address, and its reserves keyed by
/// token symbol (e.g. "NVDAx" → its reserve).
#[derive(Debug, Clone)]
pub struct KaminoCtx {
    pub program_id: Pubkey,
    pub market: Pubkey,
    pub reserves: HashMap<String, ReserveInfo>,
}

/// Load the xStocks market + its reserves from chain (borrow APY, liq threshold,
/// available liquidity per reserve).
///
/// TODO(2b.1 cont.): implement via RPC `getAccountInfo` on the market + each reserve,
/// parsing the `Reserve` struct at the IDL-defined offsets (mirror how `src/dex/`
/// decodes on-chain account state). Needs the live IDL for the byte layout.
pub fn load_market(_rpc_url: &str, _market: &Pubkey) -> anyhow::Result<KaminoCtx> {
    unimplemented!("2b.1: RPC reserve discovery — parse Reserve accounts per the klend IDL")
}

/// Read the live health factor of an obligation (collateral×liq_threshold ÷ debt).
///
/// TODO(2b.3): parse the `Obligation` account's deposited-value / borrowed-value at
/// the IDL offsets; cross-check against `sim::estimate_health_factor`.
pub fn read_obligation_health(_rpc_url: &str, _obligation: &Pubkey) -> anyhow::Result<f64> {
    unimplemented!("2b.3: parse Obligation deposited/borrowed value per the klend IDL")
}

// Task 2b.2 — instruction builders (deposit / withdraw / borrow / repay +
// refresh_reserve / refresh_obligation). Each returns a
// `solana_sdk::instruction::Instruction` built like the MarginFi ones in
// `src/flash_loan/mod.rs`: `data = anchor_discriminator(ix_name::…) ++ borsh(args)`,
// `accounts` in the exact order the IDL specifies. The account lists are LONG and
// version-specific — derive them from the fetched IDL, do not hand-guess.

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn program_id_parses() {
        assert_eq!(program_id(), Pubkey::from_str(KLEND_PROGRAM_ID).unwrap());
    }

    #[test]
    fn anchor_discriminator_is_deterministic_8_bytes() {
        let a = anchor_discriminator(ix_name::BORROW);
        let b = anchor_discriminator(ix_name::BORROW);
        assert_eq!(a, b, "same name → same discriminator");
        assert_eq!(a.len(), 8);
        assert_ne!(
            anchor_discriminator(ix_name::BORROW),
            anchor_discriminator(ix_name::REPAY),
            "distinct instructions → distinct discriminators"
        );
    }

    #[test]
    fn obligation_pda_is_deterministic_and_owner_sensitive() {
        let pid = program_id();
        let market = Pubkey::new_unique();
        let alice = Pubkey::new_unique();
        let bob = Pubkey::new_unique();
        assert_eq!(
            obligation_pda(&alice, &market, &pid),
            obligation_pda(&alice, &market, &pid),
            "same owner+market → same obligation"
        );
        assert_ne!(
            obligation_pda(&alice, &market, &pid),
            obligation_pda(&bob, &market, &pid),
            "different owners → different obligations"
        );
    }
}
