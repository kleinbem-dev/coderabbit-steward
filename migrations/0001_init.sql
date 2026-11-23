-- Initial schema for coderabbit-steward (Phase 1+)

CREATE TABLE IF NOT EXISTS repos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    visibility TEXT NOT NULL DEFAULT 'public',
    stars INTEGER NOT NULL DEFAULT 0,
    default_branch TEXT NOT NULL DEFAULT 'main',
    has_coderabbit INTEGER NOT NULL DEFAULT 1,
    file_cap INTEGER NOT NULL DEFAULT 50,
    updated_at TEXT NOT NULL,
    UNIQUE(owner, name)
);

CREATE TABLE IF NOT EXISTS observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL,
    kind TEXT NOT NULL, -- 'check_run', 'comment', 'review', 'pull_request', 'timeline'
    github_id INTEGER NOT NULL,
    pr_number INTEGER,
    payload TEXT NOT NULL,
    seen_at TEXT NOT NULL,
    parsed INTEGER NOT NULL DEFAULT 0,
    parse_outcome TEXT,
    UNIQUE(repo, kind, github_id)
);

CREATE INDEX IF NOT EXISTS idx_observations_repo_seen ON observations(repo, seen_at);

CREATE TABLE IF NOT EXISTS spends (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL,
    identity TEXT NOT NULL DEFAULT 'unknown',
    at TEXT NOT NULL,
    cause TEXT NOT NULL DEFAULT 'unknown',
    pr INTEGER,
    check_run_id INTEGER,
    state TEXT NOT NULL, -- 'provisional', 'confirmed'
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_spends_repo_at ON spends(repo, at);

CREATE TABLE IF NOT EXISTS budget_state (
    repo TEXT PRIMARY KEY,
    pool TEXT NOT NULL DEFAULT 'pooled',
    limit_per_hour INTEGER NOT NULL DEFAULT 1,
    refill_minutes INTEGER NOT NULL DEFAULT 60,
    remaining INTEGER NOT NULL DEFAULT 1,
    next_free_at TEXT,
    confidence TEXT NOT NULL DEFAULT 'inferred', -- 'observed', 'inferred', 'unknown'
    fleet_cooldown_until TEXT,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL,
    kind TEXT NOT NULL, -- 'on-demand', 'proactive', 'full'
    pr_number INTEGER,
    paths_json TEXT,
    priority INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'waiting', -- 'waiting', 'triggered', 'reviewing', 'done', 'failed', 'merged-unreviewed'
    not_before TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_queue_repo_state ON queue(repo, state);

CREATE TABLE IF NOT EXISTS review_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL,
    pr_number INTEGER NOT NULL,
    head_sha TEXT NOT NULL,
    kind TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    outcome TEXT
);

CREATE TABLE IF NOT EXISTS file_coverage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL,
    path TEXT NOT NULL,
    reviewed_blob TEXT,
    reviewed_commit TEXT,
    by TEXT,
    unreviewed_lines INTEGER NOT NULL DEFAULT 0,
    unreviewed_since TEXT,
    excluded INTEGER NOT NULL DEFAULT 0,
    UNIQUE(repo, path)
);

CREATE TABLE IF NOT EXISTS scan_cursor (
    repo TEXT PRIMARY KEY,
    scanned_through TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS findings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL,
    path TEXT NOT NULL,
    line INTEGER,
    commit_sha TEXT,
    severity TEXT NOT NULL,
    category TEXT NOT NULL DEFAULT 'general',
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'new', -- 'new', 'real', 'false', 'fixed', 'needs-martin'
    review_run INTEGER,
    issue_number INTEGER,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_findings_repo_status ON findings(repo, status);

CREATE TABLE IF NOT EXISTS finding_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    finding_id INTEGER NOT NULL REFERENCES findings(id) ON DELETE CASCADE,
    at TEXT NOT NULL,
    from_status TEXT NOT NULL,
    to_status TEXT NOT NULL,
    actor TEXT NOT NULL,
    ref_url TEXT
);

CREATE TABLE IF NOT EXISTS demand_hist (
    repo TEXT NOT NULL,
    hour_of_week INTEGER NOT NULL,
    labelled_prs REAL NOT NULL DEFAULT 0.0,
    PRIMARY KEY(repo, hour_of_week)
);

CREATE TABLE IF NOT EXISTS settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    mode TEXT NOT NULL DEFAULT 'observe', -- 'observe', 'on-demand', 'full'
    repo_daily_cap INTEGER NOT NULL DEFAULT 6,
    fleet_daily_cap INTEGER NOT NULL DEFAULT 40,
    quiet_minutes INTEGER NOT NULL DEFAULT 45,
    pooled INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL
);

INSERT OR IGNORE INTO settings (id, mode, repo_daily_cap, fleet_daily_cap, quiet_minutes, pooled, updated_at)
VALUES (1, 'observe', 6, 40, 45, 1, '2026-11-23T00:00:00Z');

CREATE TABLE IF NOT EXISTS poll_etags (
    url TEXT PRIMARY KEY,
    etag TEXT NOT NULL,
    last_polled_at TEXT NOT NULL
);
