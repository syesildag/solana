use std::collections::HashMap;
use solana_sdk::pubkey::Pubkey;
use yellowstone_grpc_proto::geyser::{
    CommitmentLevel, SubscribeRequest, SubscribeRequestFilterAccounts,
    SubscribeRequestFilterTransactions,
};

/// Builds a SubscribeRequest that watches a set of account pubkeys for updates
/// AND subscribes to confirmed transactions that touch any of those accounts.
///
/// The transaction subscription enables the whale back-run path: when a large
/// confirmed swap is detected, BF is poked immediately without waiting for the
/// normal debounce window. `watch_vaults` should be the same vault pubkeys used
/// for the account subscription — pass `&[]` to disable transaction watching.
pub fn build_account_subscription(accounts: &[Pubkey]) -> SubscribeRequest {
    build_subscription(accounts, accounts)
}

pub fn build_subscription(accounts: &[Pubkey], watch_vaults: &[Pubkey]) -> SubscribeRequest {
    let account_filter = SubscribeRequestFilterAccounts {
        account: accounts.iter().map(|p| p.to_string()).collect(),
        owner: vec![],
        filters: vec![],
        ..Default::default()
    };

    let mut account_filters = HashMap::new();
    account_filters.insert("pools".to_string(), account_filter);

    // Transaction filter: receive confirmed (non-vote, non-failed) transactions
    // that touch any of our tracked vault accounts. Used for whale detection.
    let mut tx_filters = HashMap::new();
    if !watch_vaults.is_empty() {
        tx_filters.insert("whale".to_string(), SubscribeRequestFilterTransactions {
            vote:             Some(false),
            failed:           Some(false),
            account_include:  watch_vaults.iter().map(|p| p.to_string()).collect(),
            account_exclude:  vec![],
            account_required: vec![],
            ..Default::default()
        });
    }

    SubscribeRequest {
        accounts:           account_filters,
        slots:              HashMap::new(),
        transactions:       tx_filters,
        blocks:             HashMap::new(),
        blocks_meta:        HashMap::new(),
        entry:              HashMap::new(),
        commitment:         Some(CommitmentLevel::Processed as i32),
        accounts_data_slice: vec![],
        ping:               None,
        transactions_status: HashMap::new(),
        from_slot:          None,
    }
}
