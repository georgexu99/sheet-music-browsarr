-- Phase 1: queue, library, audit log.

CREATE TABLE queue_items (
  id INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  source TEXT NOT NULL,
  source_url TEXT NOT NULL,
  state TEXT NOT NULL,
  qbit_hash TEXT,
  local_path TEXT,
  size_bytes INTEGER,
  progress REAL,
  error TEXT,
  triggered_by_ip TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE INDEX idx_queue_items_state ON queue_items(state);
CREATE INDEX idx_queue_items_created_at ON queue_items(created_at);

CREATE TABLE library_items (
  id INTEGER PRIMARY KEY,
  queue_item_id INTEGER REFERENCES queue_items(id),
  title TEXT NOT NULL,
  path TEXT NOT NULL,
  composer TEXT,
  instrument TEXT,
  size_bytes INTEGER NOT NULL,
  public_visible INTEGER NOT NULL DEFAULT 1,
  added_at TEXT NOT NULL
);

CREATE INDEX idx_library_items_added_at ON library_items(added_at);
CREATE INDEX idx_library_items_title ON library_items(title);

CREATE TABLE audit_log (
  id INTEGER PRIMARY KEY,
  ts TEXT NOT NULL,
  ip TEXT NOT NULL,
  user_agent TEXT,
  action TEXT NOT NULL,
  target TEXT,
  result TEXT NOT NULL,
  meta TEXT
);

CREATE INDEX idx_audit_log_ts ON audit_log(ts);
CREATE INDEX idx_audit_log_ip ON audit_log(ip);
