-- Phase 2: settings, rate limits, banned IPs.

CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL,
  encrypted INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL
);

CREATE TABLE rate_buckets (
  bucket_key TEXT NOT NULL,
  day TEXT NOT NULL,
  count INTEGER NOT NULL,
  PRIMARY KEY (bucket_key, day)
);

CREATE INDEX idx_rate_buckets_day ON rate_buckets(day);

CREATE TABLE banned_ips (
  ip TEXT PRIMARY KEY,
  reason TEXT,
  banned_at TEXT NOT NULL
);
