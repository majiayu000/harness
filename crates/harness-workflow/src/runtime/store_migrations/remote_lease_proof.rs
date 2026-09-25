pub(super) const SQL: &str = "-- Claims and renewals update runtime_jobs before touching renewal
  -- receipts. Take the same table-lock order so an old instance
  -- cannot deadlock this migration while both are in flight.
  LOCK TABLE runtime_jobs IN ACCESS EXCLUSIVE MODE;
  CREATE TABLE IF NOT EXISTS runtime_job_lease_issuances (
    runtime_job_id TEXT NOT NULL
      REFERENCES runtime_jobs(id) ON DELETE CASCADE,
    lease_generation BIGINT NOT NULL,
    owner TEXT NOT NULL,
    lease_expires_at TIMESTAMPTZ NOT NULL,
    -- NULL is reserved for the exact lease that was already
    -- running when v28 was installed. Its first successful
    -- renewal rotates to a proof-bearing issuance. Every issuance
    -- created after this migration receives a random proof.
    lease_proof UUID DEFAULT gen_random_uuid(),
    -- This provenance survives proof rotation so an exact legacy receipt can
    -- still authorize a response-loss replay. New v28 issuances are false.
    legacy_proofless BOOLEAN NOT NULL DEFAULT FALSE,
    issued_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (runtime_job_id, lease_generation, owner, lease_expires_at)
  );
  CREATE INDEX IF NOT EXISTS idx_runtime_job_lease_issuances_owner
    ON runtime_job_lease_issuances (owner, runtime_job_id, lease_generation);
  ALTER TABLE runtime_job_lease_renewal_receipts
    ADD COLUMN IF NOT EXISTS legacy_proofless BOOLEAN NOT NULL DEFAULT FALSE;
  -- The runtime_jobs lock above is held until this migration
  -- commits, closing the backfill-to-trigger installation window.
  INSERT INTO runtime_job_lease_issuances
    (runtime_job_id, lease_generation, owner, lease_expires_at, lease_proof,
     legacy_proofless)
  SELECT id,
         COALESCE((data->>'lease_generation')::bigint, 0),
         data #>> '{lease,owner}',
         (data #>> '{lease,expires_at}')::timestamptz,
         NULL::uuid,
         TRUE
  FROM runtime_jobs
  WHERE runtime_kind = 'remote_host'
    AND status = 'running'
    AND data #>> '{lease,owner}' IS NOT NULL
    AND data #>> '{lease,expires_at}' IS NOT NULL
  ON CONFLICT DO NOTHING;
  INSERT INTO runtime_job_lease_issuances
    (runtime_job_id, lease_generation, owner, lease_expires_at, issued_at)
  SELECT receipt.runtime_job_id,
         receipt.lease_generation,
         receipt.owner,
         receipt.renewed_expires_at,
         receipt.created_at
  FROM runtime_job_lease_renewal_receipts AS receipt
  JOIN runtime_jobs AS job ON job.id = receipt.runtime_job_id
  WHERE job.runtime_kind = 'remote_host'
  ON CONFLICT DO NOTHING;
  -- A v27 renewal may have committed immediately before migration while its
  -- response was lost. Bind only the receipt that produced the current lease
  -- to the bounded proofless replay path; a new renewal_id is not authorized.
  UPDATE runtime_job_lease_renewal_receipts AS receipt
  SET legacy_proofless = TRUE
  FROM runtime_jobs AS job
  WHERE job.id = receipt.runtime_job_id
    AND job.runtime_kind = 'remote_host'
    AND job.status = 'running'
    AND receipt.owner = job.data #>> '{lease,owner}'
    AND receipt.lease_generation =
        COALESCE((job.data->>'lease_generation')::bigint, 0)
    AND receipt.renewed_expires_at =
        (job.data #>> '{lease,expires_at}')::timestamptz;
  -- Old binaries neither return nor validate lease proofs. Reject their
  -- lease-changing writes after migration. Proof-aware v28 transactions opt
  -- in with a transaction-local marker, so old servers fail closed.
  CREATE OR REPLACE FUNCTION enforce_remote_lease_proof_writer()
  RETURNS TRIGGER AS $$
  DECLARE
    lease_changed BOOLEAN;
  BEGIN
    lease_changed :=
      NEW.status = 'running'
      AND (
        OLD.status IS DISTINCT FROM NEW.status
        OR OLD.data #> '{lease}' IS DISTINCT FROM NEW.data #> '{lease}'
        OR OLD.data->>'lease_generation'
           IS DISTINCT FROM NEW.data->>'lease_generation'
      );
    IF NEW.runtime_kind = 'remote_host'
       AND (
         lease_changed
         OR (
           OLD.status = 'running'
           AND NEW.status IS DISTINCT FROM OLD.status
         )
       )
       AND current_setting(
         'harness.runtime_job_lease_proof_v1', true
       ) IS DISTINCT FROM '1' THEN
      RAISE EXCEPTION
        'remote runtime job lease write requires proof-aware runtime'
        USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
  END;
  $$ LANGUAGE plpgsql;
  DROP TRIGGER IF EXISTS trg_enforce_remote_lease_proof_writer ON runtime_jobs;
  CREATE TRIGGER trg_enforce_remote_lease_proof_writer
    BEFORE UPDATE OF status, data ON runtime_jobs
    FOR EACH ROW EXECUTE FUNCTION enforce_remote_lease_proof_writer();
  CREATE OR REPLACE FUNCTION record_runtime_job_lease_issuance()
  RETURNS TRIGGER AS $$
  BEGIN
    IF NEW.runtime_kind = 'remote_host'
       AND NEW.status = 'running'
       AND NEW.data #>> '{lease,owner}' IS NOT NULL
       AND NEW.data #>> '{lease,expires_at}' IS NOT NULL THEN
      INSERT INTO runtime_job_lease_issuances
        (runtime_job_id, lease_generation, owner, lease_expires_at)
      VALUES (
        NEW.id,
        COALESCE((NEW.data->>'lease_generation')::bigint, 0),
        NEW.data #>> '{lease,owner}',
        (NEW.data #>> '{lease,expires_at}')::timestamptz
      )
      ON CONFLICT DO NOTHING;
    END IF;
    RETURN NEW;
  END;
  $$ LANGUAGE plpgsql;
  DROP TRIGGER IF EXISTS trg_runtime_job_lease_issuance ON runtime_jobs;
  CREATE TRIGGER trg_runtime_job_lease_issuance
    AFTER INSERT OR UPDATE OF status, data ON runtime_jobs
    FOR EACH ROW EXECUTE FUNCTION record_runtime_job_lease_issuance();
  ALTER TABLE runtime_job_completions_dlq
    ADD COLUMN IF NOT EXISTS lease_generation BIGINT";
