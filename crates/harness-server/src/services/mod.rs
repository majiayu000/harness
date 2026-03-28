//! Service abstractions for the harness-server orchestration layer.
//!
//! Each module defines a trait interface plus a default concrete implementation.
//! The traits enable independent testing via mock implementations without
//! constructing the full [`crate::http::AppState`].
//!
//! # Services
//! - [`ProjectService`] — project registry CRUD, path resolution, default root.
//! - [`TaskService`] — task lifecycle, stream subscriptions.
//! - [`ExecutionService`] — task enqueue: project resolution, agent dispatch,
//!   workspace allocation, concurrency management, completion callbacks.

pub mod execution;
pub mod project;
pub mod task;
