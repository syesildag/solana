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
    /// (or flash loan borrow when enable_flash_loan=true)
    pub setup_instructions: Vec<Instruction>,
    /// Instructions appended to the last swap tx: close WSOL ATA → unwrap WSOL → SOL
    /// (or flash loan repay + end + close when enable_flash_loan=true)
    pub teardown_instructions: Vec<Instruction>,
    /// Flash loan origination fee paid to MarginFi (0 when enable_flash_loan=false).
    pub flash_loan_fee_lamports: u64,
    /// True when this opportunity will be submitted directly via RPC (bypass Jito).
    /// Set when bypass_jito_bundle=true, enable_flash_loan=true, and gross margin ≤ threshold.
    pub use_direct_rpc: bool,
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
        use crate::dex::types::mint_symbol;
        // Build "SOL -[Orca]→ USDT -[Raydium]→ USDC -[Meteora]→ SOL"
        let mut parts = Vec::with_capacity(self.cycle.edges.len() * 2 + 1);
        parts.push(mint_symbol(&self.cycle.path[0]));
        for edge in &self.cycle.edges {
            parts.push(format!("-[{}]→ {}", edge.dex.short_name(), mint_symbol(&edge.to)));
        }
        let flash_str = if self.flash_loan_fee_lamports > 0 {
            format!(" | flash_fee: {}", self.flash_loan_fee_lamports)
        } else {
            String::new()
        };
        let route_str = if self.use_direct_rpc { " | route: direct-RPC" } else { "" };
        format!(
            "Cycle: {} | in: {} SOL | gross: {} | tip: {}{}{} | net: {} lamports ({:.2} bps)",
            parts.join(" "),
            self.amount_in as f64 / 1e9,
            self.gross_out,
            self.jito_tip_lamports,
            flash_str,
            route_str,
            self.net_profit_lamports,
            self.profit_bps()
        )
    }
}
