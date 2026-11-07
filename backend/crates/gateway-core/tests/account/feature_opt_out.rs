//! 显式关闭只记录已知功能，范围判断不把缺席账号当成已关闭

use std::collections::BTreeSet;
use std::sync::Arc;

use gateway_core::account::{AccountFeatureOptOuts, AccountOptionalFeature, ProviderAccountId};
use gateway_core::routing::{
    ClientRoutingScope, FrozenAccountScope, ProviderKind, RuntimeAccount, RuntimeAccountDirectory,
};

fn scope(opted_out: &str) -> FrozenAccountScope {
    let provider = ProviderKind::new("openai").unwrap();
    let directory = RuntimeAccountDirectory::new(
        ["acct_inherit", "acct_off"]
            .into_iter()
            .map(|id| {
                let mut account = RuntimeAccount::new(provider.clone(), BTreeSet::new());
                if id == opted_out {
                    account = account.with_feature_opt_outs(AccountFeatureOptOuts::new([
                        AccountOptionalFeature::CyberSessionBlock,
                    ]));
                }
                (ProviderAccountId::new(id).unwrap(), account)
            })
            .collect(),
    );
    FrozenAccountScope::new(Arc::new(directory), ClientRoutingScope::all_accounts())
}

#[test]
fn persisted_tokens_ignore_unknown_values_and_wire_tokens_reject_them() {
    let parsed = AccountFeatureOptOuts::from_persisted_tokens([
        "gpt-4o",
        "cyber_session_block",
        "capacity_freeze",
    ]);
    assert_eq!(
        parsed.tokens(),
        vec![
            "capacity_freeze".to_owned(),
            "cyber_session_block".to_owned()
        ]
    );
    assert!(AccountFeatureOptOuts::from_wire_tokens(&["gpt-4o".to_owned()]).is_err());
    assert!(
        AccountFeatureOptOuts::from_wire_tokens(&[
            "cyber_session_block".to_owned(),
            "cyber_session_block".to_owned(),
        ])
        .is_err()
    );
    assert!(
        AccountFeatureOptOuts::from_wire_tokens(&[])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn scope_excludes_inheriting_accounts_and_treats_unknown_accounts_as_protected() {
    let scope = scope("acct_off");
    let cyber = AccountOptionalFeature::CyberSessionBlock;
    let off = ProviderAccountId::new("acct_off").unwrap();
    let inherit = ProviderAccountId::new("acct_inherit").unwrap();
    let missing = ProviderAccountId::new("acct_missing").unwrap();
    assert!(scope.any_opted_out(cyber));
    assert!(scope.opts_out(&off, cyber));
    assert!(!scope.opts_out(&inherit, cyber));
    assert!(!scope.opts_out(&missing, cyber));
    let narrowed = scope.excluding_accounts_inheriting(cyber);
    assert!(narrowed.allows(&off));
    assert!(!narrowed.allows(&inherit));
    assert!(scope.allows(&inherit));
}
