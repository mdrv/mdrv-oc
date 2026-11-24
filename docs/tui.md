# The queue console

`mdrv-oc tui` is the interactive view over the prompt queue. It exists for the
jobs the CLI makes tedious: retiming a prompt, fixing a typo in one that is
already scheduled, firing one early, or copying an id without retyping it from
`list`.

Bare `mdrv-oc` (no arguments) opens the console too when both stdin and stdout
are terminals; everywhere else it prints the long help and exits 2, so shell
snippets and `mdrv-oc | less` keep working. `mdrv-oc help` is unchanged.

## Requirements

- An interactive terminal on stdin and stdout. Pipes fail with a pointer to
  `session prompt list`, and so does `--json` on the console.
- A build with the default `tui` feature (`--no-default-features` keeps the CLI
  only and makes `tui` fail with an explanation).
- Nothing else: the console is a view over `schedule.json` and needs no config.
  It delivers nothing itself — that stays the daemon's job, and the header tells
  you whether the daemon is running.

## Keyboard

| Key | Effect |
| --- | --- |
| `j` `↓`, `k` `↑` | move |
| `pgdn` / `pgup`, `ctrl+d` / `ctrl+u` | page |
| `g` / `G`, `home` / `end` | first / last |
| `e`, `enter` | edit the selected prompt |
| `d`, `del` | cancel (asks `y`) |
| `f` | fire now (pending and in-flight rows) |
| `r` | requeue as a new prompt, whatever its state |
| `y` | copy the job id (OSC 52) |
| `/` | filter on text, id and session |
| `?` | the key sheet |
| `q` | quit |

In the editor: `tab` / `shift+tab` move between prompt, session and due,
`ctrl+p` opens the server's session list, `ctrl+s` saves and `esc` leaves
(a dirty form asks first).

## Semantics worth knowing

- **Edits are guarded.** Saving carries the state the form was loaded with
  (state, text, session, due); if the daemon or another `mdrv-oc` changed the
  job meanwhile, the save is refused and the list reloads — no silent
  last-write-wins.
- **In-flight jobs are locked.** While the daemon is delivering a prompt
  (`delivering`, younger than 5 minutes) the row shows `⏳` and refuses edits;
  a `delivering` value left behind by a dead daemon ages out and becomes
  editable again.
- **Times use the CLI grammar** — `--at`-style input (`HH:MM`,
  `YYYY-MM-DD HH:MM`, `tomorrow HH:MM`, RFC3339) and `2h30m` durations. The
  field is prefilled in `YYYY-MM-DD HH:MM` form. A past time is allowed and
  fires on the next tick.
- **Sessions are free text.** `ctrl+p` picks from the running server; typing an
  id by hand works too, and an id the server does not know is accepted once the
  server is unreachable (you get a warning, not a veto).
- **Cancel asks, fire-now does not.** Cancel is destructive; a manual fire is
  what you asked for. `e` and `f` refuse a finished row and point at `r`, which
  is the way to run one again.
- **Requeue never edits history.** It copies text, session and agent/model
  under a fresh id, leaves the finished original alone, and opens the copy's
  form so the new time can be typed straight away.
- **Nothing is written on a broken queue.** If `schedule.json` cannot be read,
  the console shows the error and only `r` (reload) and `q` work.

## Terminal compatibility

- Colors follow the terminal's own theme and degrade to 16 colors;
  `NO_COLOR` is honoured. Nothing is colour-only: the selected row carries a
  `▸` caret as well as the highlight, and every state has a glyph
  (`·` `⏳` `✓` `✗`).
- The layout needs 64×12. Smaller terminals get a "terminal too small" screen
  with the current size instead of a mangled list.
- Mouse: wheel scrolls the list, a click selects a row.
- Rendering is a plain cell grid (the `nsh` terminal backend), no graphics,
  no truecolor requirement.
- Windows is supported through the same backend but has not been verified on
  a real console yet; `ctrl+s` needs a terminal that does not eat it.

## Development

The console is `src/tui/`, gated behind the default `tui` feature:

- `state.rs` — rows, selection, modes, the key map. Pure and unit tested.
- `ui.rs` — components; they draw the store and decide nothing.
- `mod.rs` — terminal setup/restore, the poll loop, and the effects
  (write, reload, clipboard). Writes go through the library's CAS helpers.

Restoration is ordered and idempotent: a `Drop` guard plus a panic hook that
both funnel into one `restore()` behind an atomic flag (mouse, paste, cursor,
alternate screen, raw mode, in that order).

Tests:

```sh
cargo test --no-default-features      # CLI only
cargo test                            # + the state machine
```

## Troubleshooting

- `the TUI needs an interactive terminal` — stdout is redirected; run it in a
  terminal or use `session prompt list`.
- `this build has the tui feature disabled` — rebuild without
  `--no-default-features`.
- `the queue could not be read` — the JSON is corrupt; `r` retries, `q` quits.
  `session prompt list` shows the same error and writes are off until it
  parses.
- Stale screen after a resize — the next key or daemon tick repaints; a
  terminal that reports no size at all can leave the layout at its minimum.
- `queue changed since it was loaded` — someone else touched the job; the list
  has reloaded, re-open the form.
