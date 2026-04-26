use std::collections::HashMap;
use solana_sdk::pubkey::Pubkey;
use yellowstone_grpc_proto::geyser::{
    CommitmentLevel, SubscribeRequest, SubscribeRequestFilterAccounts,
};

/// Builds a SubscribeRequest that watches a set of account pubkeys for updates.
/// Used to track pool vault accounts and CL pool state accounts.
pub fn build_account_subscription(accounts: &[Pubkey]) -> SubscribeRequest {
    let filter = SubscribeRequestFilterAccounts {
        account: accounts.iter().map(|p| p.to_string()).collect(),
        owner: vec![],
        filters: vec![],
        ..Default::default()
    };

    let mut account_filters = HashMap::new();
    account_filters.insert("pools".to_string(), filter);

    SubscribeRequest {
        accounts: account_filters,
        slots: HashMap::new(),
        transactions: HashMap::new(),
        blocks: HashMap::new(),
        blocks_meta: HashMap::new(),
        entry: HashMap::new(),
        commitment: Some(CommitmentLevel::Processed as i32),
        accounts_data_slice: vec![],
        ping: None,
        transactions_status: HashMap::new(),
        from_slot: None,
    }
}
