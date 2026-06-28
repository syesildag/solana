//! Pure wallet-capital accounting for the arbitrage loop: how much base-token capital is
//! spendable, and when to halt. Native and non-native bases differ — native pays gas from
//! the same balance it trades; a non-native base trades USDC but still pays SOL gas.

#[derive(Debug, PartialEq, Eq)]
pub enum HaltDecision {
    Continue,
    /// Base-token P&L dropped below the drawdown threshold (caller debounces to a halt).
    WarnPnl,
    /// Native SOL gas balance exhausted — cannot pay tips/fees (immediate halt).
    HaltGas,
}

/// Spendable base capital after reserving `overhead` and applying `input_cap`.
pub fn spendable_base(balance: u64, overhead: u64, input_cap: u64) -> u64 {
    balance.saturating_sub(overhead).min(input_cap)
}

/// Decide halt state from the latest balances. Gas guard applies only to a non-native
/// base (a native base's gas == its base balance, covered by the P&L guard).
pub fn evaluate_halt(b_base: u64, pnl_threshold: u64, b_sol: u64, gas_floor: u64, is_native: bool) -> HaltDecision {
    if !is_native && b_sol < gas_floor {
        return HaltDecision::HaltGas;
    }
    if b_base < pnl_threshold {
        return HaltDecision::WarnPnl;
    }
    HaltDecision::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spendable_subtracts_overhead_then_caps() {
        // 10 capital, 2 overhead, cap 100 → 8
        assert_eq!(spendable_base(10, 2, 100), 8);
        // cap binds
        assert_eq!(spendable_base(1_000, 0, 50), 50);
        // overhead exceeds balance → 0
        assert_eq!(spendable_base(1, 5, 100), 0);
    }

    #[test]
    fn halt_gas_only_for_non_native() {
        // non-native, SOL below gas floor → HaltGas regardless of base balance
        assert_eq!(evaluate_halt(1_000, 0, 10, 100, false), HaltDecision::HaltGas);
        // native: gas floor not separately enforced
        assert_eq!(evaluate_halt(1_000, 2_000, 10, 100, true), HaltDecision::WarnPnl);
    }

    #[test]
    fn halt_pnl_when_base_below_threshold() {
        assert_eq!(evaluate_halt(50, 100, 1_000, 100, false), HaltDecision::WarnPnl);
        assert_eq!(evaluate_halt(150, 100, 1_000, 100, false), HaltDecision::Continue);
    }
}
