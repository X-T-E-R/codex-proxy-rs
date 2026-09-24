use gateway_core::routing::{
    ModelRequestPolicy, ReasoningEffort, ReasoningEffortRule, ReasoningEffortRuleMode,
    RequestedReasoningEffort, ServiceTierRule,
};

#[test]
fn reasoning_effort_orders_from_none_to_max() {
    let mut ordered = [
        ReasoningEffort::Max,
        ReasoningEffort::None,
        ReasoningEffort::High,
        ReasoningEffort::Minimal,
        ReasoningEffort::XHigh,
        ReasoningEffort::Medium,
        ReasoningEffort::Low,
    ];
    ordered.sort();
    assert_eq!(
        ordered,
        [
            ReasoningEffort::None,
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ]
    );
}

#[test]
fn reasoning_effort_parse_round_trips_and_trims() {
    for value in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
        let effort = ReasoningEffort::parse(value).expect("parse effort");
        assert_eq!(effort.as_str(), value);
    }
    assert_eq!(
        ReasoningEffort::parse(" XHigh "),
        Some(ReasoningEffort::XHigh)
    );
    assert_eq!(ReasoningEffort::parse("ultra"), None);
}

#[test]
fn locked_rule_always_rewrites() {
    let rule = ReasoningEffortRule::new(ReasoningEffortRuleMode::Locked, ReasoningEffort::High);
    for requested in [
        RequestedReasoningEffort::Absent,
        RequestedReasoningEffort::Unknown,
        RequestedReasoningEffort::Known(ReasoningEffort::Max),
    ] {
        assert_eq!(rule.resolve(requested), Some(ReasoningEffort::High));
    }
}

#[test]
fn min_rule_raises_absent_or_lower_requests() {
    let rule = ReasoningEffortRule::new(ReasoningEffortRuleMode::Min, ReasoningEffort::High);
    assert_eq!(
        rule.resolve(RequestedReasoningEffort::Absent),
        Some(ReasoningEffort::High)
    );
    assert_eq!(rule.resolve(RequestedReasoningEffort::Unknown), None);
    assert_eq!(
        rule.resolve(RequestedReasoningEffort::Known(ReasoningEffort::Low)),
        Some(ReasoningEffort::High)
    );
    assert_eq!(
        rule.resolve(RequestedReasoningEffort::Known(ReasoningEffort::High)),
        None
    );
    assert_eq!(
        rule.resolve(RequestedReasoningEffort::Known(ReasoningEffort::Max)),
        None
    );
}

#[test]
fn max_rule_lowers_absent_or_higher_requests() {
    let rule = ReasoningEffortRule::new(ReasoningEffortRuleMode::Max, ReasoningEffort::Medium);
    assert_eq!(
        rule.resolve(RequestedReasoningEffort::Absent),
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(rule.resolve(RequestedReasoningEffort::Unknown), None);
    assert_eq!(
        rule.resolve(RequestedReasoningEffort::Known(ReasoningEffort::XHigh)),
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(
        rule.resolve(RequestedReasoningEffort::Known(ReasoningEffort::Minimal)),
        None
    );
}

#[test]
fn service_tier_rule_parses_known_values() {
    assert_eq!(
        ServiceTierRule::parse("lock_fast"),
        Some(ServiceTierRule::LockFast)
    );
    assert_eq!(
        ServiceTierRule::parse(" LOCK_NEVER_FAST "),
        Some(ServiceTierRule::LockNeverFast)
    );
    assert_eq!(ServiceTierRule::parse("fast"), None);
    assert_eq!(ServiceTierRule::LockFast.as_str(), "lock_fast");
    assert_eq!(ServiceTierRule::LockNeverFast.as_str(), "lock_never_fast");
}

#[test]
fn model_request_policy_from_facts_validates_combinations() {
    assert!(ModelRequestPolicy::from_facts(None, None, None).is_none());
    assert!(ModelRequestPolicy::from_facts(Some("locked"), None, None).is_none());
    assert!(ModelRequestPolicy::from_facts(None, Some("high"), None).is_none());
    assert!(ModelRequestPolicy::from_facts(Some("other"), Some("high"), None).is_none());
    assert!(ModelRequestPolicy::from_facts(Some("min"), Some("ultra"), None).is_none());
    assert!(ModelRequestPolicy::from_facts(None, None, Some("fast")).is_none());

    let policy = ModelRequestPolicy::from_facts(Some("min"), Some("high"), Some("lock_fast"))
        .expect("valid policy");
    let rule = policy.reasoning_effort().expect("effort rule");
    assert_eq!(rule.mode(), ReasoningEffortRuleMode::Min);
    assert_eq!(rule.value(), ReasoningEffort::High);
    assert_eq!(policy.service_tier(), Some(ServiceTierRule::LockFast));
}
