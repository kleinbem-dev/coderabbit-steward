use crate::parse::check_run::CheckRunOutcome;
use crate::parse::comment::BotMessage;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Observed,
    Inferred,
    Unknown,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Observed => "observed",
            Confidence::Inferred => "inferred",
            Confidence::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "observed" => Confidence::Observed,
            "unknown" => Confidence::Unknown,
            _ => Confidence::Inferred,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendState {
    Provisional,
    Confirmed,
}

impl SpendState {
    pub fn as_str(self) -> &'static str {
        match self {
            SpendState::Provisional => "provisional",
            SpendState::Confirmed => "confirmed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendRecord {
    pub at: Timestamp,
    pub state: SpendState,
    pub cause: String,
    pub identity: String,
    pub pr: Option<u64>,
    pub check_run_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    pub free: bool,
    pub remaining: u32,
    pub limit: u32,
    pub refill_minutes: u32,
    pub next_free_at: Option<Timestamp>,
    pub fleet_cooldown_until: Option<Timestamp>,
    pub confidence: Confidence,
}

#[derive(Debug, Clone)]
pub struct BudgetLedger {
    pub limit: u32,
    pub refill_minutes: u32,
    pub refill_samples: Vec<(Timestamp, u32)>,
    pub spends: Vec<SpendRecord>,
    pub next_free_at: Option<Timestamp>,
    pub fleet_cooldown_until: Option<Timestamp>,
    pub confidence: Confidence,
    pub last_observed_at: Option<Timestamp>,
    pub unrecognised_alert: bool,
}

impl Default for BudgetLedger {
    fn default() -> Self {
        Self {
            limit: 1,
            refill_minutes: 60,
            refill_samples: Vec::new(),
            spends: Vec::new(),
            next_free_at: None,
            fleet_cooldown_until: None,
            confidence: Confidence::Inferred,
            last_observed_at: None,
            unrecognised_alert: false,
        }
    }
}

impl BudgetLedger {
    pub fn apply_check_run(
        &mut self,
        outcome: &CheckRunOutcome,
        pr: Option<u64>,
        check_run_id: Option<u64>,
        identity: &str,
        cause: &str,
    ) {
        match outcome {
            CheckRunOutcome::InProgress { started_at } => {
                let already = self.spends.iter().any(|s| {
                    (check_run_id.is_some() && s.check_run_id == check_run_id)
                        || s.at == *started_at
                });
                if !already {
                    self.spends.push(SpendRecord {
                        at: *started_at,
                        state: SpendState::Provisional,
                        cause: cause.to_string(),
                        identity: identity.to_string(),
                        pr,
                        check_run_id,
                    });
                }
            }
            CheckRunOutcome::Reviewed { started_at, .. } => {
                if let Some(existing) = self.spends.iter_mut().find(|s| {
                    (check_run_id.is_some() && s.check_run_id == check_run_id)
                        || (s.state == SpendState::Provisional && s.at == *started_at)
                }) {
                    existing.state = SpendState::Confirmed;
                } else {
                    self.spends.push(SpendRecord {
                        at: *started_at,
                        state: SpendState::Confirmed,
                        cause: cause.to_string(),
                        identity: identity.to_string(),
                        pr,
                        check_run_id,
                    });
                }
            }
            CheckRunOutcome::RateLimited { started_at, .. } => {
                self.spends.retain(|s| {
                    !(s.state == SpendState::Provisional
                        && ((check_run_id.is_some() && s.check_run_id == check_run_id)
                            || s.at == *started_at))
                });
                let window_start =
                    *started_at - SignedDuration::from_mins(i64::from(self.refill_minutes));
                let has_recent_spend = self.spends.iter().any(|s| s.at >= window_start);
                if !has_recent_spend {
                    let until =
                        *started_at + SignedDuration::from_mins(i64::from(self.refill_minutes));
                    self.fleet_cooldown_until = Some(
                        self.fleet_cooldown_until
                            .map_or(until, |cur| cur.max(until)),
                    );
                }
            }
            CheckRunOutcome::SkippedLabel { .. }
            | CheckRunOutcome::SkippedIncremental { .. }
            | CheckRunOutcome::SkippedBotAuthor { .. }
            | CheckRunOutcome::SkippedOther { .. } => {}
            CheckRunOutcome::Unrecognised { .. } => {
                self.confidence = Confidence::Unknown;
                self.unrecognised_alert = true;
            }
        }
    }

    pub fn apply_bot_message(&mut self, msg: &BotMessage) {
        match msg {
            BotMessage::RateLimited {
                at,
                minutes_until_free,
            }
            | BotMessage::ProbeRateLimited {
                at,
                minutes_until_free,
            } => {
                let next_free = *at + SignedDuration::from_mins(i64::from(*minutes_until_free));
                self.next_free_at = Some(next_free);
                self.confidence = Confidence::Observed;
                self.last_observed_at = Some(*at);

                let lookback = *at - SignedDuration::from_mins(180);
                if let Some(latest_spend) = self
                    .spends
                    .iter()
                    .filter(|s| s.state == SpendState::Confirmed && s.at >= lookback && s.at <= *at)
                    .max_by_key(|s| s.at)
                {
                    let secs = next_free.duration_since(latest_spend.at).as_secs();
                    let mins = ((secs + 59) / 60).max(60) as u32;
                    self.record_refill_sample(*at, mins);
                } else {
                    self.fleet_cooldown_until = Some(
                        self.fleet_cooldown_until
                            .map_or(next_free, |cur| cur.max(next_free)),
                    );
                }
            }
            BotMessage::ProbeAvailableNow { at } => {
                self.next_free_at = None;
                self.fleet_cooldown_until = None;
                self.confidence = Confidence::Observed;
                self.last_observed_at = Some(*at);
            }
            BotMessage::ReviewSummary {
                at,
                limit,
                remaining_after,
                ..
            } => {
                if let Some(l) = limit {
                    self.limit = (*l).max(1);
                }
                self.confidence = Confidence::Observed;
                self.last_observed_at = Some(*at);
                if *remaining_after == Some(0) && self.next_free_at.is_none() {
                    let base = self
                        .spends
                        .iter()
                        .filter(|s| s.state == SpendState::Confirmed)
                        .map(|s| s.at)
                        .max()
                        .unwrap_or(*at);
                    self.next_free_at =
                        Some(base + SignedDuration::from_mins(i64::from(self.refill_minutes)));
                }
            }
            BotMessage::SkippedBotAuthor { .. }
            | BotMessage::Skipped { .. }
            | BotMessage::ReviewFinished { .. }
            | BotMessage::StatusComment { .. } => {}
            BotMessage::Unrecognised { .. } => {
                self.confidence = Confidence::Unknown;
                self.unrecognised_alert = true;
            }
        }
    }

    fn record_refill_sample(&mut self, at: Timestamp, minutes: u32) {
        let cutoff = at - SignedDuration::from_hours(24 * 7);
        self.refill_samples.retain(|(ts, _)| *ts >= cutoff);
        self.refill_samples.push((at, minutes));
        self.refill_minutes = self
            .refill_samples
            .iter()
            .map(|(_, m)| *m)
            .max()
            .unwrap_or(60)
            .max(60);
    }

    pub fn snapshot(&self, now: Timestamp) -> BudgetSnapshot {
        let window_start = now - SignedDuration::from_mins(i64::from(self.refill_minutes));
        let provisional_cutoff = now - SignedDuration::from_mins(15);

        let active_spends: Vec<&SpendRecord> = self
            .spends
            .iter()
            .filter(|s| match s.state {
                SpendState::Confirmed => s.at >= window_start,
                SpendState::Provisional => s.at >= provisional_cutoff && s.at >= window_start,
            })
            .collect();

        let used = active_spends.len() as u32;
        let mut remaining = self.limit.saturating_sub(used);

        let mut effective_next_free = self.next_free_at.filter(|t| *t > now);
        if effective_next_free.is_none()
            && remaining == 0
            && let Some(oldest) = active_spends.iter().map(|s| s.at).min()
        {
            let t = oldest + SignedDuration::from_mins(i64::from(self.refill_minutes));
            if t > now {
                effective_next_free = Some(t);
            }
        }
        if effective_next_free.is_some() {
            remaining = 0;
        }

        let active_fleet_cooldown = self.fleet_cooldown_until.filter(|t| *t > now);
        if active_fleet_cooldown.is_some() {
            remaining = 0;
        }

        let effective_confidence = match self.confidence {
            Confidence::Unknown => Confidence::Unknown,
            Confidence::Observed => {
                let stale = self
                    .last_observed_at
                    .is_none_or(|t| now.duration_since(t) > SignedDuration::from_mins(60));
                if stale {
                    Confidence::Inferred
                } else {
                    Confidence::Observed
                }
            }
            Confidence::Inferred => Confidence::Inferred,
        };

        let free = remaining > 0
            && effective_confidence != Confidence::Unknown
            && active_fleet_cooldown.is_none();

        BudgetSnapshot {
            free,
            remaining,
            limit: self.limit,
            refill_minutes: self.refill_minutes,
            next_free_at: effective_next_free.or(active_fleet_cooldown),
            fleet_cooldown_until: active_fleet_cooldown,
            confidence: effective_confidence,
        }
    }
}
