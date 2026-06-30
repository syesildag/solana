//! gRPC price feed for the momentum trader.
//!
//! This module provides an opt-in Yellowstone gRPC-based price update stream for the
//! momentum trader, complementing the existing Jupiter REST quote API. Configuration
//! is provided via `PortfolioConfig` fields: `momentum_grpc_pricing` (master switch),
//! `grpc_endpoint`, `grpc_token`, `pools_path` (pool metadata), and
//! `momentum_grpc_stale_secs` (staleness threshold).
//!
//! `WatchedToken` entries optionally carry `pool` (Raydium/Meteora/Orca pool pubkey)
//! and `quote` (quote token mint) for normalized pricing; these are populated by
//! later tasks.
