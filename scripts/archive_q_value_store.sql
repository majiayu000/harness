-- Archive the removed q_value_store shared schema without dropping data.
--
-- This script is intentionally not run by the application. Operators can run it
-- manually after confirming no rollback is needed.

DO $$
DECLARE
    archive_schema CONSTANT text := 'q_value_store_archived_20260704';
    archive_table text;
    archived_rows bigint;
BEGIN
    IF EXISTS (
        SELECT 1
        FROM pg_catalog.pg_namespace
        WHERE nspname = 'q_value_store'
    ) THEN
        IF EXISTS (
            SELECT 1
            FROM pg_catalog.pg_namespace
            WHERE nspname = archive_schema
        ) THEN
            RAISE NOTICE 'archive schema % already exists; leaving q_value_store unchanged',
                archive_schema;
        ELSE
            EXECUTE format('ALTER SCHEMA %I RENAME TO %I', 'q_value_store', archive_schema);
            RAISE NOTICE 'renamed q_value_store schema to %', archive_schema;
        END IF;
    ELSE
        RAISE NOTICE 'q_value_store schema is absent; nothing to archive';
    END IF;

    IF EXISTS (
        SELECT 1
        FROM pg_catalog.pg_namespace
        WHERE nspname = archive_schema
    ) THEN
        FOREACH archive_table IN ARRAY ARRAY[
            'pipeline_events',
            'rule_experiences',
            'q_value_store_legacy_backfills'
        ]
        LOOP
            IF to_regclass(format('%I.%I', archive_schema, archive_table)) IS NULL THEN
                RAISE NOTICE '%.% table is absent', archive_schema, archive_table;
            ELSE
                EXECUTE format('SELECT count(*) FROM %I.%I', archive_schema, archive_table)
                    INTO archived_rows;
                RAISE NOTICE '%.% rows: %', archive_schema, archive_table, archived_rows;
            END IF;
        END LOOP;
    END IF;
END
$$;
