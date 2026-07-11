# mdrv-oc

Inspect and manipulate the OpenCode SQLite database.

A small Rust **library + CLI** that talks directly to the OpenCode session
database (`opencode.db`) to do things the TUI doesn't expose — starting with
**moving a session to another directory**.

This exists because recent OpenCode versions removed the ability to point an
existing session at a new working directory; the only official path is a
destination picker that lists _already-known_ projects. Editing the `session`
row's `directory` (and optionally `project_id`) directly is the reliable
workaround, and `mdrv-oc` makes it safe (backup + confirm by default).

> **Warning — LLM-assisted project.** This codebase was generated with AI
> assistance and has only been tested against **OpenCode v1.17.x**. The SQLite
> schema it relies on may change without notice. OpenCode v2 is on the horizon
> and its schema is unknown at this time — this tool will almost certainly
> break on a major version bump. Use at your own risk and always keep backups.

## Install

```bash
cargo install --path .
# binary is `mdrv-oc`
```

## Quick start

```bash
# list recent sessions (newest first)
mdrv-oc session list

# show one session (exact id, id prefix, slug, or title substring)
mdrv-oc session show mighty-wizard

# move a session's directory, passing it as an argument
mdrv-oc session mv mighty-wizard /x/g/some-other-dir

# ...or be prompted for the directory interactively
mdrv-oc session mv ses_0b8dc33a7
```

All commands take `--json` / `--pretty` / `--quiet`, and `--db <PATH>` to point
at a non-default database.

## Safety

OpenCode's DB runs in WAL mode. **Quit every running OpenCode process** before
mutating it — concurrent writers have caused real corruption in the wild. By
default `session mv` writes a `<db>.bak` copy first and asks for confirmation;
pass `--yes` to confirm, `--no-backup` to skip the backup, `--dry-run` to only
print the planned change.

## Library

```rust,no_run
# use mdrv_oc::{Db, SessionSelector, session, pathutil};
# fn main() -> anyhow::Result<()> {
let mut db = Db::open_default()?;
let sess = session::resolve(&db, &SessionSelector::parse("mighty-wizard"))?;
let new_dir = pathutil::normalize_directory("/x/g/new-dir")?;
session::session_move(&mut db, &sess.id, &new_dir, None, false)?;
# Ok(())
# }
```
