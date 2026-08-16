// ===========================================================================
// `mdrv-oc` — inspect and manipulate the OpenCode SQLite database.
//
// This file is the *crate root* for the library. Its `pub mod` / `pub use`
// declarations define the public surface — what users get when they write
//     use mdrv_oc::{Db, SessionSelector};
//
// Module map:
//   db.rs       — locate/open/backup the SQLite DB; owns the connection.
//   model.rs    — plain data types: Session, Project.
//   pathutil.rs — expand `~`, normalize a directory string.
//   session.rs  — read sessions, resolve a selector, rewrite a session dir.
//   project.rs  — read projects, infer the project_id for a directory.
//   transfer.rs — bulk session export/import by driving the `opencode` CLI.
//   error.rs    — the narrow, typed error used by library internals.
//
// The CLI lives separately under `src/main.rs` + `src/cli.rs` and is a thin
// adapter over this library (no business logic in the binary).
// ===========================================================================

#![doc = include_str!("../README.md")]

pub mod db;
pub mod error;
pub mod model;
pub mod pathutil;
pub mod project;
pub mod session;
pub mod transfer;

// Convenience re-exports — the types most callers reach for first. This is the
// Rust equivalent of a barrel file (`index.ts`).
pub use db::Db;
pub use error::{Error, Result};
pub use model::{Project, Session};
pub use session::{MoveResult, SessionSelector};

// Crate version, baked in at compile time. The `clap` CLI uses the same trick
// for `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
