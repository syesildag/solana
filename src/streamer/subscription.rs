use std::collections::HashMap;
use solana_sdk::pubkey::Pubkey;
use yellowstone_grpc_proto::geyser::{
    subscribe_request_filter_accounts_filter::Filter as AccFilter,
    subscribe_request_filter_accounts_filter_memcmp::Data as MemcmpData,
    CommitmentLevel, SubscribeRequest, SubscribeRequestFilterAccounts,
    SubscribeRequestFilterAccountsFilter, SubscribeRequestFilterAccountsFilterMemcmp,
    SubscribeRequestFilterTransactions,
};

const METEORA_DLMM_PROGRAM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";

/// Builds a SubscribeRequest that watches a set of account pubkeys for updates
/// AND subscribes to confirmed transactions that touch any of those accounts.
///
/// The transaction subscription enables the whale back-run path: when a large
/// confirmed swap is detected, BF is poked immediately without waiting for the
/// normal debounce window. `watch_vaults` should be the same vault pubkeys used
/// for the account subscription — pass `&[]` to disable transaction watching.
///
/// `dlmm_lb_pairs`: one owner+memcmp account filter is added per DLMM pool so
/// every BinArray of that pool (lb_pair at offset 24) streams automatically —
/// the fill walk's data source. Pass `&[]` to skip.
pub fn build_account_subscription(accounts: &[Pubkey], dlmm_lb_pairs: &[Pubkey]) -> SubscribeRequest {
    build_subscription(accounts, accounts, dlmm_lb_pairs)
}

pub fn build_subscription(
    accounts: &[Pubkey],
    watch_vaults: &[Pubkey],
    dlmm_lb_pairs: &[Pubkey],
) -> SubscribeRequest {
    let account_filter = SubscribeRequestFilterAccounts {
        account: accounts.iter().map(|p| p.to_string()).collect(),
        owner: vec![],
        filters: vec![],
        ..Default::default()
    };

    let mut account_filters = HashMap::new();
    account_filters.insert("pools".to_string(), account_filter);

    // One owner+memcmp filter per DLMM pool: every BinArray account carries its
    // lb_pair at offset 24, so this streams ALL bin arrays of the pool — the
    // active bin migrating across array boundaries needs no resubscribe.
    for lb_pair in dlmm_lb_pairs {
        account_filters.insert(
            format!("bins:{lb_pair}"),
            SubscribeRequestFilterAccounts {
                account: vec![],
                owner: vec![METEORA_DLMM_PROGRAM.to_string()],
                filters: vec![SubscribeRequestFilterAccountsFilter {
                    filter: Some(AccFilter::Memcmp(SubscribeRequestFilterAccountsFilterMemcmp {
                        offset: 24,
                        data: Some(MemcmpData::Bytes(lb_pair.to_bytes().to_vec())),
                    })),
                }],
                ..Default::default()
            },
        );
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn bin_filters_added_per_dlmm_pool() {
        let acc = Pubkey::new_unique();
        let lb_pair = Pubkey::from_str("HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR").unwrap();
        let req = build_account_subscription(&[acc], &[lb_pair]);
        assert!(req.accounts.contains_key("pools"));
        let key = format!("bins:{lb_pair}");
        let f = req.accounts.get(&key).expect("bin filter present");
        assert_eq!(f.owner, vec![METEORA_DLMM_PROGRAM.to_string()]);
        assert_eq!(f.filters.len(), 1);
        match f.filters[0].filter.as_ref().expect("filter set") {
            AccFilter::Memcmp(m) => {
                assert_eq!(m.offset, 24);
                assert_eq!(m.data, Some(MemcmpData::Bytes(lb_pair.to_bytes().to_vec())));
            }
            _ => panic!("expected memcmp filter"),
        }
        // no-DLMM call sites keep the old shape
        let req2 = build_account_subscription(&[acc], &[]);
        assert_eq!(req2.accounts.len(), 1);
    }
}
