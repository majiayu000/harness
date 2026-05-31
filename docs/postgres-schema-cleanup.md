# Postgres Path-Derived Schema Cleanup

This workflow is for legacy `h[0-9a-f]{16}` schemas created from store identity
paths. It is intentionally conservative: dry-run first, registry-backed drops
only, and explicit allowlisting for any unregistered schema.

## Preconditions

1. Run from the same host namespace that owns the Harness workspace paths.
2. Confirm the database URL via config or `HARNESS_DATABASE_URL`.
3. Back up candidate schemas before any destructive cleanup:

```bash
pg_dump "$HARNESS_DATABASE_URL" --schema <schema> --file backups/<schema>.sql
```

## Dry Run

```bash
harness-pg-schema-cleanup --include-unregistered
```

The report classifies schemas as:

- `KEEP`: registered path-derived schema, or a registered schema missing enough
  ownership data for automated cleanup.
- `DROP_CANDIDATE`: registered non-path-derived schema whose owner path is
  missing.
- `BLOCKED`: unregistered path-derived schema. It is never dropped unless named
  with `--allow-unregistered`.

Table counts and estimated row counts are included so operators can compare the
plan against backups and production expectations.

## Apply

After reviewing backups, live traffic, and the dry-run output, name the exact
registered drop-candidate schema(s) to remove:

```bash
harness-pg-schema-cleanup --apply --confirm-drop --schema h0123456789abcdef
```

Unregistered schemas require an explicit allowlist:

```bash
harness-pg-schema-cleanup --apply --confirm-drop --allow-unregistered h0123456789abcdef
```

`--apply` validates the full request before any destructive operation, then uses
`DROP SCHEMA "<schema>" CASCADE` and removes registry rows for registered schemas
it dropped. If a registered schema is not a drop candidate, is not registered,
or is unregistered and not explicitly allowlisted, cleanup refuses to drop it.

## Rollback Boundary

Cleanup is destructive after `DROP SCHEMA CASCADE`. Restore by replaying the
matching `pg_dump` output into the same database and then re-running the dry-run
to confirm the restored schema is visible.
