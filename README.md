# mdrv-oc

Inspect and manipulate the OpenCode SQLite database.

A small Rust **library + CLI** that talks directly to the OpenCode session
database (`opencode.db`) to do things the TUI doesn't expose — moving a session
to another directory, and **bulk export/import of sessions** for sharing work
between devices.

This exists because recent OpenCode versions removed the ability to point an
existing session at a new working directory; the only official path is a
destination picker that lists _already-known_ projects. Editing the `session`
row's `directory` (and optionally `project_id`) directly is the reliable
workaround, and `mdrv-oc` makes it safe (backup + confirm by default).

> **Warning — LLM-assisted project.** This codebase was generated with AI
> assistance and has been tested against **OpenCode v1.17.x–1.18.x and
> v2.0.x**. The SQLite schema it relies on may change without notice; the
> transfer subcommands are located automatically on both major versions
> (`opencode export` on v1, `opencode session export` on v2). Use at your own
> risk and always keep backups.

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

# export sessions: selectors, --filter, or an interactive multi-select menu
mdrv-oc session export mighty-wizard clever-garden --out ~/transfer
mdrv-oc session export --filter "libhogweed" --out ~/transfer --yes
mdrv-oc session export --out ~/transfer          # numbered menu: "1,3-5" or "all"

# see what's inside the sync dir before importing (no DB writes)
mdrv-oc session inspect /x/m/oc
2026-09-22  ses_f5a323ad…  Implementasi awal chat app…  /x/g/ny26  (243.1 KB)  [in db]  <- 2026-09-22_clever-canyon_ses_f5a323ad.json.zst

# import on the other device (from the project's directory!):
cd /x/g/my-project && mdrv-oc session import ~/transfer --yes
opencode -s ses_0b8dc33a7                        # resume where you left off
```

All commands take `--json` / `--pretty` / `--quiet`, and `--db <PATH>` to point
at a non-default database.

## Session transfer

OpenCode ships one-session-at-a-time transfer commands, but only one at a time
— and their location moved in v2 (`opencode export <id>` on v1.17–1.18,
`opencode session export <id>` on v2; mdrv-oc detects which via `--version`).
`mdrv-oc` wraps them for bulk transfer and keeps the format exactly what your
installed `opencode` produces/expects (the schema drifts between versions, so
mdrv-oc never serializes sessions itself).

- **export** writes one self-describing file per session into `--out` (default
  `.`), named `YYYY-MM-DD_<slug>_ses_<id8>.json.zst` — the session's _start_
  date in your local timezone, its slug, and the first 8 chars of the id
  (plain `.json` with `--no-compress`; session JSON compresses 4–8x). Custom
  names: `--name "my-name"` on a single-session export, or answer
  `custom file names?` in the interactive batch flow (empty = default).
- **Re-exports converge.** The out dir is indexed by the session id found
  _inside_ each file — file naming doesn't matter. A newer session overwrites
  its older copy, an equal copy is reported `up-to-date` and skipped, and an
  on-disk copy _newer_ than the DB row is kept. When one session already has
  several copies under different names, all of them are listed as a reminder.
- **import** takes files _or_ directories (a directory covers every `*.json` /
  `*.json.zst` inside). `.json.zst` files are detected by magic bytes and
  decompressed transparently — no extra flags. Invalid files are skipped with
  a warning. The preview dedupes multiple files of the same session (newest
  wins, others shown as `dup`), compares each against the DB (rows: `[new]`,
  `[newer — refresh]` selected by default; `[up-to-date]`, `[older — skip]`,
  `[dup — skip]` skipped by default), and asks `import which?` — answer `y`
  for the recommended set, `all` for everything, or a row spec like `1,3-5`.
- **inspect** — `mdrv-oc session inspect <file|dir>...` prints what each
  export contains (date, id, title, working directory, size, `[in db]`) by
  streaming only the `info` object out of the (transparently decompressed)
  file. No DB writes, no `opencode` subprocess.
- Like upstream import, an imported session is **re-anchored to the project of
  your current working directory** — the original machine's paths are ignored,
  so no path remapping is needed. Run the import from inside the project.
- **Gotcha (v1.18.x):** `opencode export` truncates at exactly 128 KiB when its
  stdout is a pipe, so mdrv-oc redirects the child's stdout straight into the
  output file — don't pipe `opencode export` yourself.
- Requires the `opencode` binary in `PATH` (override with `--opencode-bin`);
  a `--db` override is propagated to the child via `OPENCODE_DB`.
- **OpenCode v2 note:** upstream v2 keeps sessions in its own `session_v2`
  table and stops writing the legacy `session` one. mdrv-oc reads
  `session_v2` whenever the table exists (v1 databases only have `session`),
  so v2-era sessions — including fresh imports — are listed, resolvable, and
  exportable. `session move` writes the active table and mirrors the change
  into the legacy one when it still holds the row.
- Not carried by the export format: todos, share URLs, pending inputs,
  `parent_id` linkage. None of these block continuing a session.

## Safety

OpenCode's DB runs in WAL mode. **Quit every running OpenCode process** before
mutating it — concurrent writers have caused real corruption in the wild. By
default `session mv` and `session import` write a `<db>.bak` copy first and ask
for confirmation; pass `--yes` to confirm, `--no-backup` to skip the backup,
`--dry-run` (mv only) to only print the planned change. `session export` never
writes to the DB.

## Shell completion (carapace)

Dynamic completion is provided via a hand-crafted carapace spec
([carapace](https://carapace.sh) must be installed). Session selectors
complete to live `id — slug — title` candidates from the DB, `project show`
completes project ids, and paths/directories complete on `mv`/`export`/`import`
slots. Install with:

```sh
mkdir -p ~/.config/carapace/specs
mdrv-oc completion > ~/.config/carapace/specs/mdrv-oc.yaml
```

The spec is baked into the binary (`include_str!` of `specs/mdrv-oc.yaml`), so
re-run the install line after upgrading to stay in sync. Dynamic candidates
come from the hidden `mdrv-oc __complete <sessions|projects>` subcommand;
completion always targets the default DB (`--db` is not visible to the `$()`
exec macro). Note for `session import`: complete `.json`/`.zst` files or
directories from the project root on the target device.

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
