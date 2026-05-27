-- Phase 0: admin authentication only.
-- Additional tables (settings, indexers, queue_items, library_items,
-- audit_log, rate_buckets, banned_ips) ship in their own migrations
-- as their owning phases land.

CREATE TABLE admin_user (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  password_hash TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
