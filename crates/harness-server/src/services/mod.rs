//! Service abstractions for the harness-server orchestration layer.
//!
//! Each module defines a trait interface plus a default concrete implementation.
//! The traits enable independent testing via mock implementations without
//! constructing the full [`crate::http::AppState`].
//!
//! # Freeze (GH-1976)
//!
//! This surface is **frozen**. Do not add new `*_svc` traits, methods, or
//! default impls, and do not migrate the many handlers that already use
//! `state.core.*` merely to improve the call-site ratio. Prefer the dominant
//! `state.core.*` pattern for new handlers until a concrete execution,
//! isolation, or testing need requires a service boundary. Coordinate any
//! future unfreeze with dual-lifecycle work (#1958) and RuntimeContext
//! extraction (#1798) instead of overlapping bulk migrations.
//!
//! # Services
//! - [`ProjectService`] — project registry CRUD, path resolution, default root.
//! - [`TaskService`] — task lifecycle, stream subscriptions.
//! - [`ExecutionService`] — task enqueue: project resolution, agent dispatch,
//!   workspace allocation, concurrency management, completion callbacks.

pub mod execution;
pub mod project;
pub mod task;
