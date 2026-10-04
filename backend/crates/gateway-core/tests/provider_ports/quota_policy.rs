use gateway_core::provider_ports::quota_policy::{QuotaAction, QuotaPolicy, QuotaPolicyMode};

#[test]
fn quota_policy_defaults_are_off_with_conservative_automatic_limits() {
    let policy = QuotaPolicy::default();
    assert!(!policy.enabled());
    assert!(policy.valid());
    assert_eq!(policy.auto_reset.max_attempts_per24h, 1);
    assert_eq!(policy.auto_reset.cooldown_seconds, 86_400);
    assert_eq!(QuotaPolicyMode::default(), QuotaPolicyMode::Inherit);
    let value = serde_json::to_value(policy).unwrap();
    assert_eq!(value["autoReset"]["maxAttemptsPer24h"], 1);
    assert_eq!(value["secondary"]["action"], "off");
}

#[test]
fn quota_rules_validate_exact_boundaries_and_separate_roles() {
    let mut policy = QuotaPolicy::default();
    policy.secondary.action = QuotaAction::Stop;
    for threshold in [1, 99, 100] {
        policy.secondary.threshold_percent = threshold;
        assert!(policy.valid());
        assert!(policy.enabled());
        assert_eq!(policy.primary.action, QuotaAction::Off);
    }
    for threshold in [0, 101, 255] {
        policy.secondary.threshold_percent = threshold;
        assert!(!policy.valid());
    }
    policy.secondary.threshold_percent = 99;
    for (budget, interval, valid) in [
        (1, 86400, true),
        (10, 3600, true),
        (0, 86400, false),
        (11, 86400, false),
        (1, 3599, false),
        (1, 86401, false),
    ] {
        policy.auto_reset.max_attempts_per24h = budget;
        policy.auto_reset.cooldown_seconds = interval;
        assert_eq!(policy.valid(), valid);
    }
}

#[test]
fn quota_policy_wire_rejects_unknown_actions_fields_and_negative_thresholds() {
    let value = serde_json::to_value(QuotaPolicy::default()).unwrap();
    for (field, bad) in [
        ("action", serde_json::json!("consume_all")),
        ("thresholdPercent", serde_json::json!(-1)),
    ] {
        let mut invalid = value.clone();
        invalid["secondary"][field] = bad;
        assert!(serde_json::from_value::<QuotaPolicy>(invalid).is_err());
    }
    let mut invalid = value;
    invalid["creditsBalanceTrigger"] = serde_json::json!(true);
    assert!(serde_json::from_value::<QuotaPolicy>(invalid).is_err());
}
