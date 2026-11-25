use coderabbit_steward::db::Db;
use coderabbit_steward::findings::{
    compute_fingerprint, parse_outside_diff_findings, parse_review_comment,
    render_findings_issue_body,
};
use serde_json::json;

#[test]
fn parses_coderabbit_inline_comment_and_outside_diff() {
    let comment = json!({
        "user": { "login": "coderabbitai[bot]" },
        "path": "src/main.rs",
        "line": 42,
        "diff_hunk": "@@ -40,3 +40,5 @@\n+let x = unwrap();",
        "body": "**Major** · **Functional Correctness**\n\n**Avoid panicking unwrap on user input.**\n\nUse `?` instead."
    });
    let f = parse_review_comment(&comment).expect("should parse CodeRabbit comment");
    assert_eq!(f.path, "src/main.rs");
    assert_eq!(f.line, Some(42));
    assert_eq!(f.severity, "major");
    assert_eq!(f.category, "correctness");
    assert_eq!(f.title, "Avoid panicking unwrap on user input.");

    let non_cr = json!({
        "user": { "login": "kleinbem" },
        "path": "src/main.rs",
        "body": "LGTM"
    });
    assert!(parse_review_comment(&non_cr).is_none());

    let review_body = "Outside diff range comments:\n\n`src/auth.rs` (12-18):\n\n**Critical** · **Security**\n\n**Missing constant-time check.**\n\nUse `ct_eq`.";
    let outside = parse_outside_diff_findings(review_body);
    assert_eq!(outside.len(), 1);
    assert_eq!(outside[0].path, "src/auth.rs");
    assert_eq!(outside[0].line, Some(12));
    assert_eq!(outside[0].severity, "critical");
    assert_eq!(outside[0].category, "security");
}

#[tokio::test]
async fn finding_upsert_preserves_triage_status_and_renders_issue() {
    let dir = tempfile::tempdir().unwrap();
    let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("f.db").display());
    let db = Db::connect(&db_url).await.unwrap();

    let comment = json!({
        "user": { "login": "coderabbitai[bot]" },
        "path": "src/api.rs",
        "line": 100,
        "diff_hunk": "fn handler() {}",
        "body": "**Minor**\n\n**Tighten CORS policy.**\n\nDetails."
    });
    let parsed = parse_review_comment(&comment).unwrap();
    let fp1 = compute_fingerprint("kleinbem/demo", &parsed.path, &parsed.title, &parsed.code_quote);
    let fp2 = compute_fingerprint("kleinbem/demo", &parsed.path, "  tighten  cors policy. ", "fn handler() {}\n");
    assert_eq!(fp1, fp2);

    let (id1, is_new1) = db
        .upsert_finding("kleinbem/demo", &parsed, Some("sha1"), None)
        .await
        .unwrap();
    assert!(is_new1);

    // Mark as false alarm, then re-upsert same finding -> status must stay 'false'
    assert!(db.update_finding_status(id1, "false", "martin").await.unwrap());
    let (id2, is_new2) = db
        .upsert_finding("kleinbem/demo", &parsed, Some("sha2"), None)
        .await
        .unwrap();
    assert!(!is_new2);
    assert_eq!(id1, id2);

    let rows = db.list_findings(Some("kleinbem/demo"), None).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "false");

    let body = render_findings_issue_body("kleinbem/demo", &rows);
    assert!(body.contains("Tighten CORS policy."));
    assert!(body.contains("src/api.rs:100"));
}
