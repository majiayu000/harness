use harness_core::db::Migration;

pub const EVENT_STORE_SCHEMA: &str = "event_store";

pub(super) static EVENT_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create events table with indexes",
        sql: "CREATE TABLE IF NOT EXISTS events (
        id          TEXT PRIMARY KEY,
        ts          TEXT NOT NULL,
        session_id  TEXT NOT NULL,
        hook        TEXT NOT NULL,
        tool        TEXT NOT NULL,
        decision    TEXT NOT NULL,
        reason      TEXT,
        detail      TEXT,
        duration_ms BIGINT
    );
    CREATE INDEX IF NOT EXISTS idx_events_session_id ON events (session_id);
    CREATE INDEX IF NOT EXISTS idx_events_hook ON events (hook);
    CREATE INDEX IF NOT EXISTS idx_events_decision ON events (decision);
    CREATE INDEX IF NOT EXISTS idx_events_ts ON events (ts)",
    },
    Migration {
        version: 2,
        description: "add content column to events table",
        sql: "ALTER TABLE events ADD COLUMN content TEXT",
    },
    Migration {
        version: 3,
        description: "create scan_watermarks table",
        sql: "CREATE TABLE IF NOT EXISTS scan_watermarks (
            project      TEXT NOT NULL,
            agent_id     TEXT NOT NULL,
            last_scan_ts TEXT NOT NULL,
            PRIMARY KEY (project, agent_id)
        )",
    },
    Migration {
        version: 4,
        description: "add metadata column to events table",
        sql: "ALTER TABLE events ADD COLUMN metadata TEXT",
    },
    Migration {
        version: 5,
        description: "scope events and scan watermarks within shared schema",
        sql: "ALTER TABLE events ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE events
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE events ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE events ALTER COLUMN store_key SET NOT NULL;
              ALTER TABLE events DROP CONSTRAINT IF EXISTS events_pkey;
              ALTER TABLE events ADD CONSTRAINT events_pkey PRIMARY KEY (store_key, id);
              DROP INDEX IF EXISTS idx_events_session_id;
              DROP INDEX IF EXISTS idx_events_hook;
              DROP INDEX IF EXISTS idx_events_decision;
              DROP INDEX IF EXISTS idx_events_ts;
              CREATE INDEX IF NOT EXISTS idx_events_store_session_id
              ON events (store_key, session_id);
              CREATE INDEX IF NOT EXISTS idx_events_store_hook
              ON events (store_key, hook);
              CREATE INDEX IF NOT EXISTS idx_events_store_decision
              ON events (store_key, decision);
              CREATE INDEX IF NOT EXISTS idx_events_store_ts
              ON events (store_key, ts);
              ALTER TABLE scan_watermarks ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE scan_watermarks
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE scan_watermarks ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE scan_watermarks ALTER COLUMN store_key SET NOT NULL;
              ALTER TABLE scan_watermarks DROP CONSTRAINT IF EXISTS scan_watermarks_pkey;
              ALTER TABLE scan_watermarks ADD CONSTRAINT scan_watermarks_pkey
              PRIMARY KEY (store_key, project, agent_id)",
    },
    Migration {
        version: 6,
        description: "record legacy event store backfills",
        sql: "CREATE TABLE IF NOT EXISTS event_store_legacy_backfills (
            store_key     TEXT NOT NULL DEFAULT current_schema(),
            legacy_schema TEXT NOT NULL,
            copied_rows   BIGINT NOT NULL DEFAULT 0,
            backfilled_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (store_key, legacy_schema)
        )",
    },
];
