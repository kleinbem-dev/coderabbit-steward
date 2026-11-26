-- Per-repo toggle for automatic background proactive reviews.
-- Explicit "Full Repo Review" requests still run regardless of this flag.
ALTER TABLE repos ADD COLUMN proactive_enabled INTEGER NOT NULL DEFAULT 1;
