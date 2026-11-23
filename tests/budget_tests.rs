use coderabbit_steward::budget::{BudgetLedger, Confidence};
use coderabbit_steward::parse::check_run::CheckRunOutcome;
use coderabbit_steward::parse::comment::BotMessage;
use jiff::{SignedDuration, Timestamp};

fn ts(s: &str) -> Timestamp {
    s.parse().expect("valid timestamp")
}

#[test]
fn spend_and_refill_with_fake_clock() {
    let mut ledger = BudgetLedger::default();
    let t0 = ts("2026-11-22T10:00:00Z");

    assert!(ledger.snapshot(t0).free);
    assert_eq!(ledger.snapshot(t0).remaining, 1);

    // Spend at t0
    ledger.apply_check_run(
        &CheckRunOutcome::Reviewed {
            started_at: t0,
            completed_at: Some(t0 + SignedDuration::from_mins(2)),
        },
        Some(1),
        Some(100),
        "kleinbem",
        "pr_opened_labelled",
    );

    let t_mid = t0 + SignedDuration::from_mins(30);
    let snap_mid = ledger.snapshot(t_mid);
    assert!(!snap_mid.free);
    assert_eq!(snap_mid.remaining, 0);
    assert_eq!(snap_mid.next_free_at, Some(t0 + SignedDuration::from_mins(60)));

    let t_refilled = t0 + SignedDuration::from_mins(61);
    let snap_after = ledger.snapshot(t_refilled);
    assert!(snap_after.free);
    assert_eq!(snap_after.remaining, 1);
}

#[test]
fn limit_learned_from_footer_and_slower_refill() {
    let mut ledger = BudgetLedger::default();
    let t0 = ts("2026-11-22T10:00:00Z");

    ledger.apply_check_run(
        &CheckRunOutcome::Reviewed {
            started_at: t0,
            completed_at: None,
        },
        Some(1),
        Some(1),
        "kleinbem",
        "pr",
    );
    // Footer reports limit 2, 1 remaining
    ledger.apply_bot_message(&BotMessage::ReviewSummary {
        at: t0 + SignedDuration::from_mins(2),
        actionable_comments: Some(1),
        files_processed: Some(1),
        limit: Some(2),
        remaining_after: Some(1),
        no_actionable: false,
    });
    let snap = ledger.snapshot(t0 + SignedDuration::from_mins(3));
    assert_eq!(snap.limit, 2);
    assert_eq!(snap.remaining, 1);
    assert!(snap.free);

    // Fair use slows refill: rate limit at +10m says available in 80m -> 90m total refill
    ledger.apply_bot_message(&BotMessage::RateLimited {
        at: t0 + SignedDuration::from_mins(10),
        minutes_until_free: 80,
    });
    let snap2 = ledger.snapshot(t0 + SignedDuration::from_mins(11));
    assert_eq!(snap2.refill_minutes, 90);
    assert!(!snap2.free);
}

#[test]
fn cross_repo_cooldown_when_rate_limited_without_local_spend() {
    let mut ledger = BudgetLedger::default();
    let t0 = ts("2026-11-22T10:00:00Z");

    ledger.apply_bot_message(&BotMessage::RateLimited {
        at: t0,
        minutes_until_free: 40,
    });
    let snap = ledger.snapshot(t0 + SignedDuration::from_mins(1));
    assert!(!snap.free);
    assert_eq!(
        snap.fleet_cooldown_until,
        Some(t0 + SignedDuration::from_mins(40))
    );
}

#[test]
fn provisional_spend_expires_if_never_confirmed() {
    let mut ledger = BudgetLedger::default();
    let t0 = ts("2026-11-22T10:00:00Z");

    ledger.apply_check_run(
        &CheckRunOutcome::InProgress { started_at: t0 },
        Some(1),
        Some(55),
        "kleinbem",
        "pr",
    );
    assert!(!ledger.snapshot(t0 + SignedDuration::from_mins(5)).free);
    // After 16 minutes without confirmation, provisional spend drops
    assert!(ledger.snapshot(t0 + SignedDuration::from_mins(16)).free);
}

#[test]
fn observed_confidence_becomes_inferred_after_60_minutes() {
    let mut ledger = BudgetLedger::default();
    let t0 = ts("2026-11-22T10:00:00Z");

    ledger.apply_bot_message(&BotMessage::ProbeAvailableNow { at: t0 });
    assert_eq!(
        ledger.snapshot(t0 + SignedDuration::from_mins(10)).confidence,
        Confidence::Observed
    );
    assert_eq!(
        ledger.snapshot(t0 + SignedDuration::from_mins(61)).confidence,
        Confidence::Inferred
    );
}
