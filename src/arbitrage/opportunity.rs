use solana_sdk::instruction::Instruction;

use crate::graph::bellman_ford::ArbCycle;

/// A fully evaluated arbitrage opportunity, ready for simulation and execution.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArbOpportunity {
    /// The detected cycle, e.g. [SOL, USDC, RAY, SOL]
    pub cycle: ArbCycle,
    /// Input amount in lamports (SOL)
    pub amount_in: u64,
    /// Expected gross output in lamports (before fees/tip deducted)
    pub gross_out: u64,
    /// DEX swap fees across all hops (in lamports)
    pub total_swap_fee_lamports: u64,
    /// Solana base transaction fee (5000 lamports × num_txs)
    pub tx_fee_lamports: u64,
    /// Jito tip to pay the validator (in lamports)
    pub jito_tip_lamports: u64,
    /// Net profit = gross_out − amount_in − total_swap_fee − tx_fee − jito_tip
    pub net_profit_lamports: i64,
    /// Per-hop swap instructions (one per hop in the cycle)
    pub swap_instructions: Vec<Instruction>,
    /// Minimum output required at each hop (slippage guard)
    pub minimum_outputs: Vec<u64>,
    /// Instructions prepended to tx[0]: create intermediate ATAs + wrap SOL → WSOL
    pub setup_instructions: Vec<Instruction>,
    /// Instructions appended to the last swap tx: close WSOL ATA → unwrap WSOL → SOL
    pub teardown_instructions: Vec<Instruction>,
}

impl ArbOpportunity {
    #[allow(dead_code)]
    pub fn is_profitable(&self) -> bool {
        self.net_profit_lamports > 0
    }

    pub fn profit_bps(&self) -> f64 {
        if self.amount_in == 0 {
            return 0.0;
        }
        self.net_profit_lamports as f64 / self.amount_in as f64 * 10_000.0
    }

    pub fn summary(&self) -> String {
        let path: Vec<String> = self.cycle.path.iter().map(|p| p.to_string()[..8].to_string()).collect();
        format!(
            "Cycle: {} | in: {} SOL | gross: {} | net: {} lamports ({:.2} bps)",
            path.join("→"),
            self.amount_in as f64 / 1e9,
            self.gross_out,
            self.net_profit_lamports,
            self.profit_bps()
        )
    }
}
