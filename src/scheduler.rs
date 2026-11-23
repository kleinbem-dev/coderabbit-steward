use crate::budget::{BudgetSnapshot, Confidence};
use crate::config::Mode;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueCandidate {
    pub id: i64,
    pub pr_number: Option<u64>,
    pub risky_paths: bool,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoScheduleInput {
    pub repo: String,
    pub mode: Mode,
    pub budget: BudgetSnapshot,
    pub has_pending_trigger: bool,
    pub pending_timed_out: bool,
    pub on_demand_queue: Vec<QueueCandidate>,
    pub quiet_for_minutes: u32,
    pub required_quiet_minutes: u32,
    pub predicted_demand: f64,
    pub open_unlabelled_risky_pr: bool,
    pub proactive_today_repo: u32,
    pub repo_daily_cap: u32,
    pub proactive_today_fleet: u32,
    pub fleet_daily_cap: u32,
    pub has_proactive_candidate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledAction {
    WaitPending,
    TimeoutPending,
    TriggerOnDemand { queue_id: i64, pr_number: Option<u64> },
    TriggerProactive,
    Idle { reason: String },
}

/// Pure 1-minute tick scheduler decision for one repository.
pub fn decide(input: &RepoScheduleInput) -> ScheduledAction {
    if input.pending_timed_out {
        return ScheduledAction::TimeoutPending;
    }
    if input.has_pending_trigger {
        return ScheduledAction::WaitPending;
    }

    if !input.on_demand_queue.is_empty() && input.budget.free {
        let mut sorted = input.on_demand_queue.clone();
        sorted.sort_by(|a, b| {
            b.risky_paths
                .cmp(&a.risky_paths)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
        let first = &sorted[0];
        if input.mode == Mode::Observe {
            return ScheduledAction::Idle {
                reason: format!(
                    "observe: would trigger on-demand queue item {} (PR {:?})",
                    first.id, first.pr_number
                ),
            };
        }
        return ScheduledAction::TriggerOnDemand {
            queue_id: first.id,
            pr_number: first.pr_number,
        };
    }

    if !input.on_demand_queue.is_empty() {
        return ScheduledAction::Idle {
            reason: "on-demand queued, waiting for budget".to_string(),
        };
    }

    if input.mode != Mode::Full {
        return ScheduledAction::Idle {
            reason: format!("mode is {}", input.mode.as_str()),
        };
    }

    if !input.has_proactive_candidate {
        return ScheduledAction::Idle {
            reason: "no proactive candidate".to_string(),
        };
    }
    if !input.budget.free || input.budget.confidence == Confidence::Unknown {
        return ScheduledAction::Idle {
            reason: "budget not free or confidence unknown".to_string(),
        };
    }
    if input.budget.limit > 1 && input.budget.remaining <= 1 {
        return ScheduledAction::Idle {
            reason: "keeping 1 slot in reserve".to_string(),
        };
    }
    if input.quiet_for_minutes < input.required_quiet_minutes {
        return ScheduledAction::Idle {
            reason: "repo not quiet long enough".to_string(),
        };
    }
    if input.predicted_demand >= 0.15 {
        return ScheduledAction::Idle {
            reason: "predicted demand too high".to_string(),
        };
    }
    if input.open_unlabelled_risky_pr {
        return ScheduledAction::Idle {
            reason: "open unlabelled risky PR present".to_string(),
        };
    }
    if input.proactive_today_repo >= input.repo_daily_cap
        || input.proactive_today_fleet >= input.fleet_daily_cap
    {
        return ScheduledAction::Idle {
            reason: "daily proactive cap reached".to_string(),
        };
    }

    ScheduledAction::TriggerProactive
}
