-- ============================================================================
-- 0001_schema.sql — complete schema for remon-server
-- ============================================================================
-- Design notes:
-- 1. Single-host: one server instance monitors the machine it runs on.
--    Multi-host is handled at the frontend layer (connect to multiple instances).
-- 2. Time-series rolled up into resolution buckets ('raw','1m','5m','1h')
--    via a self-organizing rollup task (see services/rollup.rs).
-- 3. Retention is per (resource, resolution) and configurable at runtime
--    via the retention_policy table.
-- 4. Server-side runtime configuration lives in server_config; TOML defaults
--    are layered under DB values (DB always wins) at boot.
-- 5. WITHOUT ROWID where the primary key fully covers row identity, to skip
--    the extra rowid index.
-- 6. Foreign keys are enforced (set by SqliteConnectOptions::foreign_keys).
-- ============================================================================

-- ─── DEVICES & SESSIONS (mobile clients) ────────────────────────────────────
CREATE TABLE devices (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    token_hash        TEXT NOT NULL,
    totp_secret       TEXT,
    fcm_token         TEXT,
    web_push_endpoint TEXT,
    web_push_p256dh   TEXT,
    web_push_auth     TEXT,
    last_ip           TEXT,
    last_seen         INTEGER NOT NULL DEFAULT (unixepoch()),
    created_at        INTEGER NOT NULL DEFAULT (unixepoch()),
    is_active         INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE sessions (
    id         TEXT PRIMARY KEY,
    device_id  TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX idx_sessions_device  ON sessions(device_id);
CREATE INDEX idx_sessions_expires ON sessions(expires_at);
CREATE INDEX idx_devices_active_fcm ON devices(is_active, fcm_token);

-- VAPID identifies the server to the push relay. Same keypair across all
-- subscribers, generated once on first boot if missing. Stored as PEM —
-- private for signing the push JWT, public also kept here so we can serve
-- it to clients without re-deriving every time.
CREATE TABLE vapid_keys (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    public_key  TEXT    NOT NULL,
    private_key TEXT    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch())
);

-- ─── RUNTIME CONFIG ─────────────────────────────────────────────────────────
CREATE TABLE server_config (
    id                              INTEGER PRIMARY KEY CHECK (id = 1),
    server_name                     TEXT NOT NULL DEFAULT 'My Server',
    collector_stats_interval_ms     INTEGER NOT NULL DEFAULT 2000,
    collector_processes_interval_ms INTEGER NOT NULL DEFAULT 5000,
    collector_docker_interval_ms    INTEGER NOT NULL DEFAULT 3000,
    rollup_tick_interval_ms         INTEGER NOT NULL DEFAULT 60000,
    retention_tick_interval_ms      INTEGER NOT NULL DEFAULT 3600000,
    updated_at                      INTEGER NOT NULL DEFAULT (unixepoch())
);
INSERT INTO server_config (id) VALUES (1);

-- ─── RESOLUTIONS ────────────────────────────────────────────────────────────
CREATE TABLE resolutions (
    name             TEXT PRIMARY KEY,
    interval_seconds INTEGER NOT NULL,
    rollup_from      TEXT REFERENCES resolutions(name),
    enabled          INTEGER NOT NULL DEFAULT 1,
    sort_order       INTEGER NOT NULL
);
INSERT INTO resolutions (name, interval_seconds, rollup_from, enabled, sort_order) VALUES
    ('raw', 2,    NULL,  1, 0),
    ('1m',  60,   'raw', 1, 1),
    ('5m',  300,  '1m',  1, 2),
    ('1h',  3600, '5m',  1, 3);

-- ─── RETENTION POLICY ───────────────────────────────────────────────────────
CREATE TABLE retention_policy (
    resource     TEXT NOT NULL,
    resolution   TEXT NOT NULL,
    keep_seconds INTEGER NOT NULL,
    PRIMARY KEY (resource, resolution)
) WITHOUT ROWID;

INSERT INTO retention_policy (resource, resolution, keep_seconds) VALUES
    ('cpu',          'raw', 86400),
    ('cpu',          '1m',  604800),
    ('cpu',          '5m',  2592000),
    ('cpu',          '1h',  31536000),
    ('cpu_cores',    'raw', 86400),
    ('memory',       'raw', 86400),
    ('memory',       '1m',  604800),
    ('memory',       '5m',  2592000),
    ('memory',       '1h',  31536000),
    ('disk',         'raw', 86400),
    ('disk',         '1m',  604800),
    ('disk',         '5m',  2592000),
    ('disk',         '1h',  31536000),
    ('network',      'raw', 86400),
    ('network',      '1m',  604800),
    ('network',      '5m',  2592000),
    ('network',      '1h',  31536000),
    ('docker',       'raw', 86400),
    ('docker',       '1m',  604800),
    ('docker',       '5m',  2592000),
    ('docker',       '1h',  31536000),
    ('pressure',     'raw', 86400),
    ('pressure',     '1m',  604800),
    ('pressure',     '5m',  2592000),
    ('pressure',     '1h',  31536000),
    ('components',   'raw', 86400),
    ('components',   '1m',  604800),
    ('components',   '5m',  2592000),
    ('components',   '1h',  31536000),
    ('logs',         'raw', 2592000),
    ('probe_runs',   'raw', 2592000),
    ('probe',        'raw', 86400),
    ('probe',        '1m',  604800),
    ('probe',        '5m',  2592000),
    ('probe',        '1h',  31536000),
    ('alert_events', 'raw', 7776000);

-- ─── ROLLUP STATE ───────────────────────────────────────────────────────────
CREATE TABLE rollup_state (
    resource       TEXT NOT NULL,
    resolution     TEXT NOT NULL,
    last_bucket_ts INTEGER NOT NULL DEFAULT 0,
    last_run_at    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (resource, resolution)
) WITHOUT ROWID;

-- ─── METRICS — host-level ───────────────────────────────────────────────────
CREATE TABLE metrics_cpu (
    resolution                TEXT    NOT NULL REFERENCES resolutions(name),
    timestamp                 INTEGER NOT NULL,
    usage_percent             REAL    NOT NULL,
    load_1m                   REAL    NOT NULL,
    load_5m                   REAL    NOT NULL,
    load_15m                  REAL    NOT NULL,
    steal_percent             REAL,
    iowait_percent            REAL,
    guest_percent             REAL,
    context_switches_per_sec  INTEGER,
    process_forks_per_sec     INTEGER,
    PRIMARY KEY (resolution, timestamp)
) WITHOUT ROWID;

CREATE TABLE metrics_cpu_cores (
    timestamp     INTEGER NOT NULL,
    core_index    INTEGER NOT NULL,
    usage_percent REAL    NOT NULL,
    freq_mhz      INTEGER NOT NULL,
    PRIMARY KEY (timestamp, core_index)
) WITHOUT ROWID;

CREATE TABLE metrics_memory (
    resolution                 TEXT    NOT NULL REFERENCES resolutions(name),
    timestamp                  INTEGER NOT NULL,
    used_bytes                 INTEGER NOT NULL,
    available_bytes            INTEGER NOT NULL,
    cached_bytes               INTEGER NOT NULL,
    swap_used_bytes            INTEGER NOT NULL,
    page_faults_minor_per_sec  INTEGER,
    page_faults_major_per_sec  INTEGER,
    swap_in_pages_per_sec      INTEGER,
    swap_out_pages_per_sec     INTEGER,
    PRIMARY KEY (resolution, timestamp)
) WITHOUT ROWID;

CREATE TABLE metrics_disk (
    resolution          TEXT    NOT NULL REFERENCES resolutions(name),
    timestamp           INTEGER NOT NULL,
    mount_point         TEXT    NOT NULL,
    used_bytes          INTEGER NOT NULL,
    available_bytes     INTEGER NOT NULL,
    read_bytes_per_sec  INTEGER NOT NULL DEFAULT 0,
    write_bytes_per_sec INTEGER NOT NULL DEFAULT 0,
    inode_used_percent  REAL,
    PRIMARY KEY (resolution, timestamp, mount_point)
) WITHOUT ROWID;

CREATE TABLE metrics_network (
    resolution         TEXT    NOT NULL REFERENCES resolutions(name),
    timestamp          INTEGER NOT NULL,
    interface_name     TEXT    NOT NULL,
    rx_bytes_per_sec   INTEGER NOT NULL,
    tx_bytes_per_sec   INTEGER NOT NULL,
    rx_packets_per_sec INTEGER NOT NULL,
    tx_packets_per_sec INTEGER NOT NULL,
    errors_in_per_sec  INTEGER NOT NULL DEFAULT 0,
    errors_out_per_sec INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (resolution, timestamp, interface_name)
) WITHOUT ROWID;

CREATE TABLE metrics_docker (
    resolution         TEXT    NOT NULL REFERENCES resolutions(name),
    timestamp          INTEGER NOT NULL,
    container_id       TEXT    NOT NULL,
    cpu_percent        REAL    NOT NULL,
    memory_used_bytes  INTEGER NOT NULL,
    memory_limit_bytes INTEGER NOT NULL,
    network_rx_bytes   INTEGER NOT NULL,
    network_tx_bytes   INTEGER NOT NULL,
    block_read_bytes   INTEGER NOT NULL DEFAULT 0,
    block_write_bytes  INTEGER NOT NULL DEFAULT 0,
    pids               INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (resolution, timestamp, container_id)
) WITHOUT ROWID;

CREATE TABLE metrics_components (
    resolution    TEXT    NOT NULL REFERENCES resolutions(name),
    timestamp     INTEGER NOT NULL,
    label         TEXT    NOT NULL,
    temperature_c REAL,
    max_c         REAL,
    critical_c    REAL,
    PRIMARY KEY (resolution, timestamp, label)
) WITHOUT ROWID;

CREATE TABLE metrics_pressure (
    resolution  TEXT    NOT NULL REFERENCES resolutions(name),
    timestamp   INTEGER NOT NULL,
    resource    TEXT    NOT NULL CHECK (resource IN ('cpu','memory','io')),
    some_avg10  REAL    NOT NULL,
    some_avg60  REAL    NOT NULL,
    some_avg300 REAL    NOT NULL,
    full_avg10  REAL    NOT NULL,
    full_avg60  REAL    NOT NULL,
    full_avg300 REAL    NOT NULL,
    PRIMARY KEY (resolution, timestamp, resource)
) WITHOUT ROWID;

-- ─── METRICS — probes ───────────────────────────────────────────────────────
-- `labels` is a canonicalised JSON object (sorted keys, no whitespace).
-- Empty labels map to '{}' so the PK is well-defined.
CREATE TABLE metrics_probe (
    resolution  TEXT    NOT NULL REFERENCES resolutions(name),
    timestamp   INTEGER NOT NULL,
    probe_name  TEXT    NOT NULL,
    metric_name TEXT    NOT NULL,
    labels      TEXT    NOT NULL DEFAULT '{}',
    value       REAL    NOT NULL,
    PRIMARY KEY (resolution, timestamp, probe_name, metric_name, labels)
) WITHOUT ROWID;

CREATE INDEX idx_metrics_probe_lookup ON metrics_probe(probe_name, metric_name, timestamp DESC);

-- ─── LOGS ───────────────────────────────────────────────────────────────────
CREATE TABLE logs (
    id        INTEGER PRIMARY KEY,
    timestamp INTEGER NOT NULL,
    level     INTEGER NOT NULL,
    source    TEXT    NOT NULL,
    target    TEXT    NOT NULL,
    message   TEXT    NOT NULL
);
CREATE INDEX idx_logs_ts     ON logs(timestamp DESC);
CREATE INDEX idx_logs_source ON logs(source, timestamp DESC);
CREATE INDEX idx_logs_level  ON logs(level, timestamp DESC);

-- ─── PROBES ─────────────────────────────────────────────────────────────────
-- Source of truth = YAML manifest on disk. This table shadows it so the
-- scheduler can track enabled state and detect manifest drift via hash.
CREATE TABLE probe_definitions (
    name           TEXT PRIMARY KEY,
    enabled        INTEGER NOT NULL DEFAULT 0,
    schedule       TEXT NOT NULL,
    timeout_ms     INTEGER NOT NULL DEFAULT 30000,
    manifest_hash  TEXT NOT NULL,
    last_loaded_at INTEGER NOT NULL DEFAULT (unixepoch())
) WITHOUT ROWID;

-- Run-meta only: did the script execute cleanly?
-- exit_code = NULL  → killed by us on timeout
-- exit_code = 0     → success, metrics are in metrics_probe
-- exit_code != 0    → script failure; metrics may still exist if it emitted any
-- parse_ok = false  → runner read no well-formed JSON line (contract violation)
CREATE TABLE probe_runs (
    id         INTEGER PRIMARY KEY,
    probe_name TEXT    NOT NULL REFERENCES probe_definitions(name) ON DELETE CASCADE,
    timestamp  INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    exit_code  INTEGER,
    message    TEXT,
    parse_ok   INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_probe_runs_name_ts ON probe_runs(probe_name, timestamp DESC);
CREATE INDEX idx_probe_runs_ts      ON probe_runs(timestamp DESC);

-- ─── ALERT ENGINE ───────────────────────────────────────────────────────────
-- Expression syntax: <metric_ref> <comparator> <number>
--   metric_ref = ns "." field [ "{" k="v",... "}" ]
-- Examples:
--   cpu.usage_percent > 80
--   disk.used_bytes{mount_point="/"} > 1073741824
--   probe.free_pct{probe_name="disk-free-root"} < 10
CREATE TABLE alert_rules (
    id                 INTEGER PRIMARY KEY,
    name               TEXT    NOT NULL,
    description        TEXT,
    enabled            INTEGER NOT NULL DEFAULT 1,
    expression         TEXT    NOT NULL,
    severity           TEXT    NOT NULL CHECK (severity IN ('warn','crit')),
    for_duration_secs  INTEGER NOT NULL DEFAULT 30,
    eval_interval_secs INTEGER NOT NULL DEFAULT 10
                         CHECK (eval_interval_secs >= 3 AND eval_interval_secs <= 3600),
    cooldown_secs      INTEGER NOT NULL DEFAULT 900,
    -- Unix epoch seconds; NULL = not silenced. Evaluator gates Fired
    -- notifications when `now < silenced_until`; state transitions and
    -- event history continue regardless. Resolved notifications are NOT
    -- silenced — recovery is always delivered.
    silenced_until     INTEGER,
    created_at         INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at         INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX idx_alert_rules_enabled ON alert_rules(enabled);

-- One row per (rule, label_set). A label-less rule produces one row keyed by
-- label_set='{}'. A rule on a multi-target metric (e.g. per-disk) produces one
-- row per distinct label set — each target's lifecycle is independent.
-- Lifecycle: ok → pending (for_duration not elapsed) → firing → ok
CREATE TABLE alert_state (
    rule_id          INTEGER NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
    label_set        TEXT    NOT NULL DEFAULT '{}',
    state            TEXT    NOT NULL CHECK (state IN ('ok','pending','firing')),
    state_since      INTEGER NOT NULL DEFAULT (unixepoch()),
    last_value       REAL,
    last_eval_at     INTEGER NOT NULL DEFAULT (unixepoch()),
    last_notified_at INTEGER,
    PRIMARY KEY (rule_id, label_set)
) WITHOUT ROWID;

CREATE INDEX idx_alert_state_state ON alert_state(state);

-- Append-only audit log of every state transition.
-- `notified` is false on cooldown-suppressed fires (still recorded for audit).
CREATE TABLE alert_events (
    id           INTEGER PRIMARY KEY,
    rule_id      INTEGER NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
    label_set    TEXT    NOT NULL DEFAULT '{}',
    event_type   TEXT    NOT NULL CHECK (event_type IN ('fired','resolved')),
    severity     TEXT    NOT NULL CHECK (severity IN ('warn','crit')),
    occurred_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    metric_value REAL,
    notified     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_alert_events_rule ON alert_events(rule_id, occurred_at DESC);
CREATE INDEX idx_alert_events_ts   ON alert_events(occurred_at DESC);

-- ─── NOTIFICATION CHANNELS ──────────────────────────────────────────────────
-- Credentials live in server config / env vars — never here.
-- `config` shape by type:
--   fcm:      {}   (targets come from devices.fcm_token)
--   telegram: {"chat_id": "-1001234..."}
--   ntfy:     {"server": "https://ntfy.sh", "topic": "my-alerts"}
--   webhook:  {"url": "https://hooks.example.com/..."}
--   web-push: {}   (targets come from devices.web_push_*)
-- `min_severity` NULL = all severities; 'warn' = warn+crit; 'crit' = crit only.
CREATE TABLE notification_channels (
    id           INTEGER PRIMARY KEY,
    name         TEXT    NOT NULL,
    type         TEXT    NOT NULL CHECK (type IN ('fcm', 'telegram', 'ntfy', 'webhook', 'web-push')),
    enabled      INTEGER NOT NULL DEFAULT 1,
    config       TEXT    NOT NULL DEFAULT '{}',
    min_severity TEXT    CHECK (min_severity IN ('warn', 'crit')),
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at   INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX idx_notification_channels_enabled ON notification_channels(enabled);
