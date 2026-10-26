//! 快速模式覆盖优先级与配额重置代次正反例

use chrono::{TimeZone as _, Utc};
use gateway_core::{account::FastMode, provider_ports::priority_policy::*};

fn observation(at: i64, revision: u64, reset: i64, used: f64) -> PriorityQuotaObservation {
    PriorityQuotaObservation {
        credential_revision: revision,
        observed_at: Utc.timestamp_opt(at, 0).unwrap(),
        windows: vec![PriorityResetWindow {
            key: "codex:18000".to_owned(),
            reset_at: Utc.timestamp_opt(reset, 0).unwrap(),
            used_percent: used,
        }],
    }
}

#[test]
fn explicit_account_off_overrides_global_on_but_cannot_bypass_group_prohibition() {
    use AccountPriorityMode as A;
    use FastMode as F;
    for (account, global, group, expected) in [
        (A::Inherit, F::Default, F::Enabled, F::Enabled),
        (A::Inherit, F::Enabled, F::Default, F::Enabled),
        (A::Disabled, F::Enabled, F::Enabled, F::Disabled),
        (A::Enabled, F::Disabled, F::Default, F::Enabled),
        (A::Enabled, F::Enabled, F::Disabled, F::Disabled),
    ] {
        assert_eq!(account.resolve(global, group), expected);
    }
}

#[test]
fn fresh_window_generation_confirms_reset_even_after_new_usage_accumulates() {
    let previous = observation(100, 1, 200, 75.0);
    for next in [
        observation(210, 1, 500, 0.0),
        observation(210, 1, 500, 7.0),
        observation(210, 1, 500, 80.0),
        observation(150, 1, 500, 7.0),
    ] {
        assert!(confirmed_reset(&previous, &next), "{next:?}");
    }
    assert!(
        confirmed_reset(
            &observation(100, 1, 200, 0.0),
            &observation(210, 1, 500, 8.0)
        ),
        "the observed old boundary also confirms a reset from an idle baseline"
    );
}

#[test]
fn stale_identity_usage_drop_without_generation_or_boundary_correction_does_not_reset() {
    let previous = observation(100, 1, 200, 75.0);
    for next in [
        observation(99, 1, 500, 0.0),
        observation(100, 1, 500, 0.0),
        observation(210, 2, 500, 0.0),
        observation(210, 1, 200, 0.0),
        observation(210, 1, 202, 0.0),
        observation(150, 1, 500, 75.0),
        observation(150, 1, 500, 80.0),
        observation(210, 1, 500, f64::NAN),
        observation(210, 1, 500, 101.0),
        observation(210, 1, 500, -1.0),
    ] {
        assert!(!confirmed_reset(&previous, &next), "{next:?}");
    }
    let mut unrelated = observation(210, 1, 500, 0.0);
    unrelated.windows[0].key = "codex:604800".to_owned();
    assert!(!confirmed_reset(&previous, &unrelated));
}
