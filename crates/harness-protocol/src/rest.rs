//! HTTP REST DTO boundary for the Harness operator/control-plane API.
//!
//! New REST request and response types belong in `harness-protocol` so CLI,
//! server, dashboard, and automation callers share one wire contract. Existing
//! server-local DTOs are legacy migration targets guarded by
//! `harness-server`'s REST DTO boundary test.
