use coderabbit_steward::budget::{BudgetLedger, SpendState};
use coderabbit_steward::parse::check_run::{CheckRunOutcome, parse_check_run};
use coderabbit_steward::parse::comment::{BotMessage, parse_issue_comment};
use coderabbit_steward::parse::review::parse_pr_review;
use jiff::Timestamp;
use serde_json::Value;
use std::fs;
use std::path::Path;

fn case_dir(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cases")
        .join(name)
}

fn load_expected() -> Value {
    let raw = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cases/expected.json"),
    )
    .expect("read expected.json");
    serde_json::from_str(&raw).expect("parse expected.json")
}

fn replay_case(name: &str) -> (Vec<CheckRunOutcome>, Vec<BotMessage>, BudgetLedger) {
    let dir = case_dir(name);
    let mut outcomes = Vec::new();
    let mut messages = Vec::new();
    let mut ledger = BudgetLedger::default();

    let check_runs_raw = fs::read_to_string(dir.join("check_runs.jsonl")).expect("check_runs.jsonl");
    for line in check_runs_raw.lines().filter(|l| !l.trim().is_empty()) {
        if let Some(outcome) = parse_check_run(line) {
            let v: Value = serde_json::from_str(line).unwrap();
            let id = v.get("id").and_then(Value::as_u64);
            ledger.apply_check_run(&outcome, None, id, "kleinbem", "pr");
            outcomes.push(outcome);
        }
    }

    let comments_raw = fs::read_to_string(dir.join("comments.json")).expect("comments.json");
    let comments: Vec<Value> = serde_json::from_str(&comments_raw).expect("comments array");
    for c in comments {
        let raw = serde_json::to_string(&c).unwrap();
        if let Some(msg) = parse_issue_comment(&raw) {
            assert!(
                !matches!(msg, BotMessage::Unrecognised { .. }),
                "case {name}: unrecognised comment: {}",
                c.get("body").and_then(Value::as_str).unwrap_or("")
            );
            ledger.apply_bot_message(&msg);
            messages.push(msg);
        }
    }

    let reviews_raw = fs::read_to_string(dir.join("reviews.json")).expect("reviews.json");
    let reviews: Vec<Value> = serde_json::from_str(&reviews_raw).expect("reviews array");
    for r in reviews {
        let raw = serde_json::to_string(&r).unwrap();
        if let Some(msg) = parse_pr_review(&raw) {
            assert!(
                !matches!(msg, BotMessage::Unrecognised { .. }),
                "case {name}: unrecognised review: {}",
                r.get("body").and_then(Value::as_str).unwrap_or("")
            );
            ledger.apply_bot_message(&msg);
            messages.push(msg);
        }
    }

    (outcomes, messages, ledger)
}

#[test]
fn replay_all_recorded_cases() {
    let expected = load_expected();
    let cases = expected.as_object().expect("expected.json object");
    assert_eq!(cases.len(), 9, "expected all 9 recorded cases");

    for (name, spec) in cases {
        let (outcomes, messages, ledger) = replay_case(name);

        let expected_spends = spec["spends"].as_array().unwrap();
        let confirmed: Vec<_> = ledger
            .spends
            .iter()
            .filter(|s| s.state == SpendState::Confirmed)
            .collect();
        assert_eq!(
            confirmed.len(),
            expected_spends.len(),
            "case {name}: spend count mismatch"
        );
        for (actual, exp) in confirmed.iter().zip(expected_spends.iter()) {
            let exp_at: Timestamp = exp["at"].as_str().unwrap().parse().unwrap();
            assert_eq!(actual.at, exp_at, "case {name}: spend timestamp mismatch");
        }

        if let Some(exp_limit) = spec.get("limit").and_then(Value::as_u64) {
            assert_eq!(u64::from(ledger.limit), exp_limit, "case {name}: limit mismatch");
        }

        if let Some(exp_refill) = spec.get("refill_minutes").and_then(Value::as_u64) {
            assert_eq!(
                u64::from(ledger.refill_minutes),
                exp_refill,
                "case {name}: refill_minutes mismatch"
            );
        }

        if let Some(exp_crs) = spec.get("check_runs").and_then(Value::as_array) {
            // Note: check_runs.jsonl only contains the latest check run per head SHA,
            // while expected.json may list superseded runs from timeline.
            assert!(
                !outcomes.is_empty() || exp_crs.is_empty(),
                "case {name}: expected check runs"
            );
            let last_exp = exp_crs.last().unwrap();
            let last_actual = outcomes.last().unwrap();
            let exp_outcome = last_exp["outcome"].as_str().unwrap();
            match (exp_outcome, last_actual) {
                ("reviewed", CheckRunOutcome::Reviewed { .. })
                | ("rate_limited", CheckRunOutcome::RateLimited { .. })
                | ("skipped_label", CheckRunOutcome::SkippedLabel { .. })
                | ("skipped_incremental", CheckRunOutcome::SkippedIncremental { .. })
                | ("skipped_bot_author", CheckRunOutcome::SkippedBotAuthor { .. }) => {}
                other => panic!("case {name}: unexpected last check run outcome: {other:?}"),
            }
        }

        if let Some(rl) = spec.get("rate_limit") {
            let exp_mins = rl["minutes_until_free"].as_u64().unwrap() as u32;
            let found = messages.iter().any(|m| {
                matches!(
                    m,
                    BotMessage::RateLimited { minutes_until_free, .. } if *minutes_until_free == exp_mins
                )
            });
            assert!(found, "case {name}: missing RateLimited({exp_mins}) message");
        }

        if let Some(probe) = spec.get("probe") {
            let exp_mins = probe["minutes_until_free"].as_u64().unwrap() as u32;
            let found = messages.iter().any(|m| {
                matches!(
                    m,
                    BotMessage::ProbeRateLimited { minutes_until_free, .. } if *minutes_until_free == exp_mins
                )
            });
            assert!(found, "case {name}: missing ProbeRateLimited({exp_mins}) message");
        }
    }
}

#[test]
fn unrecognised_bot_comment_never_guesses() {
    let payload = r#"{
        "id": 999,
        "user": {"login": "coderabbitai[bot]"},
        "created_at": "2026-11-22T12:00:00Z",
        "body": "Some completely new CodeRabbit message format we have never seen before."
    }"#;
    let msg = parse_issue_comment(payload).expect("is coderabbitai[bot]");
    assert!(matches!(msg, BotMessage::Unrecognised { .. }));

    let mut ledger = BudgetLedger::default();
    ledger.apply_bot_message(&msg);
    let snap = ledger.snapshot("2026-11-22T12:01:00Z".parse().unwrap());
    assert!(!snap.free);
    assert_eq!(
        snap.confidence,
        coderabbit_steward::budget::Confidence::Unknown
    );
}
