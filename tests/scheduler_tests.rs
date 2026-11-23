use coderabbit_steward::budget::{BudgetSnapshot, Confidence};
use coderabbit_steward::config::Mode;
use coderabbit_steward::scheduler::{QueueCandidate, RepoScheduleInput, ScheduledAction, decide};
use jiff::Timestamp;

fn base_input() -> RepoScheduleInput {
    RepoScheduleInput {
        repo: "kleinbem/openwrt".to_string(),
        mode: Mode::Full,
        budget: BudgetSnapshot {
            free: true,
            remaining: 1,
            limit: 1,
            refill_minutes: 60,
            next_free_at: None,
            fleet_cooldown_until: None,
            confidence: Confidence::Observed,
        },
        has_pending_trigger: false,
        pending_timed_out: false,
        on_demand_queue: Vec::new(),
        quiet_for_minutes: 60,
        required_quiet_minutes: 45,
        predicted_demand: 0.02,
        open_unlabelled_risky_pr: false,
        proactive_today_repo: 0,
        repo_daily_cap: 6,
        proactive_today_fleet: 0,
        fleet_daily_cap: 40,
        has_proactive_candidate: true,
    }
}

#[test]
fn quiet_repo_runs_proactive() {
    let input = base_input();
    assert_eq!(decide(&input), ScheduledAction::TriggerProactive);
}

#[test]
fn busy_repo_skips_proactive() {
    let mut input = base_input();
    input.quiet_for_minutes = 10;
    assert!(matches!(decide(&input), ScheduledAction::Idle { .. }));
}

#[test]
fn burst_orders_risky_paths_first() {
    let t0: Timestamp = "2026-11-22T14:00:00Z".parse().unwrap();
    let t1: Timestamp = "2026-11-22T14:04:00Z".parse().unwrap();
    let mut input = base_input();
    input.mode = Mode::OnDemand;
    input.on_demand_queue = vec![
        QueueCandidate {
            id: 1,
            pr_number: Some(10),
            risky_paths: false,
            created_at: t0,
        },
        QueueCandidate {
            id: 2,
            pr_number: Some(11),
            risky_paths: true,
            created_at: t1,
        },
    ];
    assert_eq!(
        decide(&input),
        ScheduledAction::TriggerOnDemand {
            queue_id: 2,
            pr_number: Some(11)
        }
    );
}

#[test]
fn observe_mode_never_triggers() {
    let mut input = base_input();
    input.mode = Mode::Observe;
    input.on_demand_queue = vec![QueueCandidate {
        id: 1,
        pr_number: Some(10),
        risky_paths: true,
        created_at: "2026-11-22T14:00:00Z".parse().unwrap(),
    }];
    assert!(matches!(decide(&input), ScheduledAction::Idle { .. }));
}
