pub mod portfolio;

// `portfolio::feed_setup` needs the arb bot's real pool parsers (dex::types::Pool,
// vault/CL account decoders) to build live gRPC subscriptions from pools.json. These
// are the arb bot's own source files (normally owned by src/main.rs), included here
// via #[path] so the lib can reuse the real parsers without duplicating them.
// Validated closed pair: dex only cross-references crate::dex + crate::graph (the
// jupiter.rs → graph::exchange_graph edge is the only external link), so this is a
// self-contained, additive include — zero change to the arb binary (src/main.rs
// keeps its own separate `mod dex; mod graph;`). Private: only lib-internal code
// (portfolio::feed_setup) needs it.
#[path = "dex/mod.rs"]
#[allow(dead_code)]
mod dex;
#[path = "graph/mod.rs"]
#[allow(dead_code)]
mod graph;
