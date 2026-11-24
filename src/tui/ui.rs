// ---------------------------------------------------------------------------
// ui.rs — the drawing half of the TUI.
//
// A component body runs once, so everything that changes is a binding: texts
// and styles read `rev()`, and rows are `each` items. Components read the
// store and draw it; they never decide anything (that is `state.rs`) and
// never perform I/O (that is `mod.rs`).
// ---------------------------------------------------------------------------

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use nsh_base::{input, theme};
use nsh_core::geometry::px;
use nsh_core::keymap::When;
use nsh_core::prelude::*;

use super::state::{Field, Kind, Mode, Row, Store};
use super::trace;

const DONE: u32 = 0x3d9970;
const WARN: u32 = 0xc48a1a;

pub(super) type Shared = Arc<Mutex<Store>>;

/// The engine thread renders, the loop thread waits on it: the store is
/// shared by `Mutex`, not a thread-local.
static STORE: OnceLock<Shared> = OnceLock::new();

pub(super) fn set_store(store: Shared) {
    let _ = STORE.set(store);
}

fn store() -> Shared {
    STORE.get().expect("the store is set before load").clone()
}

// The engine redraws when this changes: every binding reads it, every store
// mutation bumps it.
// Set once by the component, read from any thread (the loop and the tests
// bump it from outside the engine).
static REV: OnceLock<Signal<u64>> = OnceLock::new();

// The root, so a handler can hand the keyboard back the moment a form goes.
thread_local! {
    static ROOT: std::cell::Cell<Option<ElementRef>> = const { std::cell::Cell::new(None) };
}

/// Gives the keyboard back to the root when no form is on screen: a field
/// takes it with it when its region unmounts, and a key that arrives before
/// the next render would be dropped.
pub(super) fn keep_focus(store: &Shared) {
    if matches_mode(store, Mode::is_edit) {
        return;
    }
    ROOT.with(|root| {
        if let Some(root) = root.get() {
            root.focus();
        }
    });
}

/// The sync key the loop uses to hand work to the engine.
pub(super) const SYNC: &str = "F24";

pub(super) fn bump(rev: &Signal<u64>) {
    let v = rev.get_untracked();
    trace!("[bump] -> {}", v + 1);
    rev.update(|n| *n = n.wrapping_add(1));
}

/// A tracked read: the binding that calls it re-runs on every bump.
fn rev() -> u64 {
    REV.get().map_or(0, |rev| rev.get())
}

pub(super) fn revision_signal() -> Signal<u64> {
    *REV.get().expect("the App component ran first")
}

/// The colour a row's state is drawn in, on top of its glyph.
fn kind_color(kind: Kind) -> AdaptiveColor {
    match kind {
        Kind::Delivering => nsh_core::color::rgb(WARN).into(),
        Kind::Delivered => nsh_core::color::rgb(DONE).into(),
        Kind::Failed => theme().destructive,
        Kind::Pending => theme().foreground,
    }
}

fn matches_mode(store: &Shared, want: fn(&Mode) -> bool) -> bool {
    let _ = rev();
    want(&super::lock(store).mode)
}

#[component]
pub fn App() -> impl IntoElement {
    let store = store();
    // The signal lives in the engine's graph, where the handlers that bump it
    // also run; `REV` only hands it out.
    let rev = signal(0u64);
    let _ = REV.set(rev);

    // Keys that the engine would otherwise use for focus traversal must be
    // captured here, or the form's Tab handling never sees them.
    // Nothing focuses the root by itself, and keys are dropped until it is:
    // the node is only there after the first commit, so retry on a short
    // timer instead of waiting for a bump that a key would have to trigger.
    let root = element_ref();
    ROOT.with(|slot| slot.set(Some(root)));
    try_focus(root, store.clone(), 50);
    // A focused field takes the keyboard with it when its region goes, so the
    // root asks for it back whenever the form is not the thing on screen.
    {
        let store = store.clone();
        effect(move || {
            let _ = rev.get();
            if !matches_mode(&store, Mode::is_edit) {
                root.focus();
            }
        });
    }

    // The form's own commands are shortcuts: raw keys never reach a handler
    // while a field has the focus, but shortcuts still fire there.
    for (keys, c) in [("ctrl+s", 's'), ("ctrl+p", 'p')] {
        let store = store.clone();
        on_key(keys, move |_| {
            super::send(&store, &revision_signal(), super::state::Key::Ctrl(c))
        });
    }
    // The loop's own events ride in on a synthetic key; a shortcut reaches it
    // even while a field has the focus.
    {
        let store = store.clone();
        on_key_with(
            SYNC,
            KeyOptions {
                when: When::Always,
                ..KeyOptions::default()
            },
            move |_| {
                let rev = revision_signal();
                super::drain(&store, &rev);
            },
        );
    }
    // Escape backs out of the form, so it has to win over a focused field.
    {
        let store = store.clone();
        on_key_with(
            "escape",
            KeyOptions {
                when: When::Always,
                ..KeyOptions::default()
            },
            move |_| super::send(&store, &revision_signal(), super::state::Key::Esc),
        );
    }

    view()
        .flex_col()
        .h_full()
        .bg(theme().background)
        .text_color(theme().foreground)
        .focusable(true)
        .element_ref(&root)
        .on_press(|_: &()| {})
        .on_focus({
            let store = store.clone();
            move || {
                trace!("[on_focus]");
                super::lock(&store).focused = true
            }
        })
        .on_key_down({
            let store = store.clone();
            move |event: &nsh_core::element::KeyEvent| {
                trace!(
                    "[key] {} (focused={})",
                    event.key,
                    super::lock(&store).focused
                );
                let rev = revision_signal();
                if super::dispatch(&store, &rev, event) {
                    bump(&rev);
                }
            }
        })
        .child(when(too_small(store.clone()), {
            let store = store.clone();
            move || small(store.clone())
        }))
        .child(when(nothing_missing(store.clone()), {
            let store = store.clone();
            move || {
                let inner = store.clone();
                view()
                    .flex_col()
                    .flex_1()
                    .child(header(store.clone()))
                    .child(
                        view()
                            .flex_1()
                            .flex_col()
                            .px(px(1.0))
                            .child(body(inner.clone())),
                    )
                    .child(status(inner))
            }
        }))
}

/// The narrowest terminal the fixed columns still fit in.
const MIN_COLS: usize = 64;
/// Header, one row, the status line and a notice.
const MIN_ROWS: usize = 12;

fn too_small(store: Shared) -> impl Fn() -> bool {
    move || {
        let _ = rev();
        let store = super::lock(&store);
        let small = store.width < MIN_COLS || store.height + 4 < MIN_ROWS;
        trace!("[small] {}x{} -> {small}", store.width, store.height + 4);
        small
    }
}

fn nothing_missing(store: Shared) -> impl Fn() -> bool {
    let small = too_small(store);
    move || !small()
}

fn small(shared: Shared) -> impl IntoElement {
    let size = {
        let store = shared.clone();
        move || {
            let _ = rev();
            let store = super::lock(&store);
            format!("{}x{}", store.width, store.height + 4)
        }
    };
    view()
        .flex_col()
        .flex_1()
        .items_center()
        .justify_center()
        .child(text("terminal too small").text_color(theme().foreground))
        .child(
            text(move || format!("need {MIN_COLS}x{MIN_ROWS} · this is {}", size()))
                .text_color(theme().muted_foreground),
        )
        .child(text("q quits").text_color(theme().muted_foreground))
}

fn header(shared: Shared) -> impl IntoElement {
    let counts = {
        let store = shared.clone();
        move || {
            let _ = rev();
            let store = super::lock(&store);
            let finished = store.rows.iter().filter(|r| r.kind.is_finished()).count();
            format!(
                "  {} pending · {finished} recent",
                store.rows.len() - finished
            )
        }
    };
    let daemon = {
        let store = shared.clone();
        move || {
            let _ = rev();
            if super::lock(&store).daemon {
                "daemon: running"
            } else {
                "daemon: stopped ⚠"
            }
        }
    };
    let read_only = {
        let store = shared.clone();
        when(
            move || {
                let _ = rev();
                !super::lock(&store).writable()
            },
            || text(" · read only").text_color(nsh_core::color::rgb(WARN)),
        )
    };
    let filter = {
        let store = shared.clone();
        when(
            move || {
                let _ = rev();
                !super::lock(&store).filter.is_empty()
            },
            {
                let store = shared.clone();
                move || {
                    let store = store.clone();
                    text(move || {
                        let _ = rev();
                        format!(" · filter: {}", super::lock(&store).filter)
                    })
                    .text_color(theme().muted_foreground)
                }
            },
        )
    };
    view()
        .flex_row()
        .justify_between()
        .h(px(1.0))
        .px(px(1.0))
        .bg(theme().secondary)
        .child(
            view()
                .flex_row()
                .child(text("mdrv-oc").text_color(theme().secondary_foreground))
                .child(
                    text(concat!(" v", env!("CARGO_PKG_VERSION")))
                        .text_color(theme().muted_foreground),
                )
                .child(text(counts).text_color(theme().muted_foreground))
                .child(read_only)
                .child(filter),
        )
        .child(text(daemon).text_color(move || {
            let _ = rev();
            if super::lock(&shared).daemon {
                theme().muted_foreground
            } else {
                nsh_core::color::rgb(WARN).into()
            }
        }))
}

fn body(store: Shared) -> impl IntoElement {
    trace!(
        "[body] mode={:?}",
        std::mem::discriminant(&super::lock(&store).mode)
    );
    view()
        .flex_col()
        .flex_1()
        .child(when(
            {
                let store = store.clone();
                move || matches_mode(&store, Mode::is_error)
            },
            {
                let store = store.clone();
                move || error(store.clone())
            },
        ))
        .child(when(
            {
                let store = store.clone();
                move || matches_mode(&store, |m| matches!(m, Mode::Help))
            },
            help,
        ))
        .child(when(
            {
                let store = store.clone();
                move || matches_mode(&store, Mode::is_edit)
            },
            {
                let store = store.clone();
                move || editor(store.clone())
            },
        ))
        .child(when(
            {
                let store = store.clone();
                move || matches_mode(&store, |m| matches!(m, Mode::Picker))
            },
            {
                let store = store.clone();
                move || picker(store.clone())
            },
        ))
        .child(when(
            {
                let store = store.clone();
                move || {
                    matches_mode(&store, |m| {
                        matches!(m, Mode::Browse | Mode::Filter | Mode::Confirm(_))
                    })
                }
            },
            {
                let store = store.clone();
                move || list(store.clone())
            },
        ))
}

fn list(shared: Shared) -> impl IntoElement {
    let empty = {
        let store = shared.clone();
        move || {
            let _ = rev();
            super::lock(&store).rows.is_empty()
        }
    };
    let empty_line = {
        let store = shared.clone();
        move || {
            let _ = rev();
            if super::lock(&store).filter.is_empty() {
                "no prompts scheduled — `session prompt \"text\" --in 1h` adds one"
            } else {
                "nothing matches the filter — `esc` clears it"
            }
        }
    };
    view()
        .flex_col()
        .child(when(empty, move || {
            text(empty_line.clone()).text_color(theme().muted_foreground)
        }))
        .child(each(
            {
                let store = shared.clone();
                move || {
                    let _ = rev();
                    let store = super::lock(&store);
                    store.window().1.to_vec()
                }
            },
            // Rows are keyed by their job, so a re-sort moves their nodes
            // with them; `at` is where the row sits now.
            |row| row.index,
            {
                let store = shared.clone();
                move |row: ReadSignal<Row>, at: ReadSignal<usize>| row_line(store.clone(), row, at)
            },
        ))
}

/// Caret, id, due and session columns; the prompt text gets the rest.
const FIXED_COLS: usize = 53;

fn row_line(shared: Shared, row: ReadSignal<Row>, at: ReadSignal<usize>) -> impl IntoElement {
    // `row.index` is the job's place in the queue; `at` is where the row sits
    // on screen, which is what the selection counts.
    let job = {
        let store = shared.clone();
        move || {
            let _ = rev();
            let index = row.get().index;
            super::lock(&store).queue.jobs[index].clone()
        }
    };
    let is_selected = {
        let store = shared.clone();
        move || {
            let _ = rev();
            let at = at.get();
            let store = super::lock(&store);
            store.selected == store.offset + at
        }
    };
    let bg = {
        let chosen = is_selected.clone();
        move || {
            if chosen() {
                theme().accent
            } else {
                theme().background
            }
        }
    };
    let fg = {
        let chosen = is_selected.clone();
        move || {
            if chosen() {
                theme().accent_foreground
            } else {
                theme().foreground
            }
        }
    };
    // Ids are usually `sch_` plus eight hex, but the generator can produce
    // sixteen: show a unique prefix, `y` still copies the whole id.
    let id = {
        let job = job.clone();
        let chosen = is_selected.clone();
        move || {
            let mark = if chosen() { "▸" } else { " " };
            let full = job().id;
            let mut short: String = full.chars().take(12).collect();
            if full.chars().count() > 12 {
                short.pop();
                short.push('…');
            }
            format!("{mark} {short:<12} ")
        }
    };
    let due = {
        let job = job.clone();
        move || format!("{} ", mdrv_oc::schedule::format_ms(job().due_ms))
    };
    let who = {
        let job = job.clone();
        move || {
            let job = job();
            let name = job
                .session_title
                .clone()
                .unwrap_or_else(|| job.session_id.clone());
            let mut short: String = name.chars().take(18).collect();
            if name.chars().count() > 18 {
                short.push('…');
            }
            format!("{short:<19} ")
        }
    };
    let text_line = {
        let store = shared.clone();
        let job = job.clone();
        move || {
            let room = super::lock(&store).width;
            super::state::first_line(&job().text, room.saturating_sub(FIXED_COLS))
        }
    };
    // Fixed columns keep ids, times and states from jittering between rows;
    // the caret marks the selection for terminals with one colour.
    view()
        .flex_row()
        .h(px(1.0))
        .overflow_hidden()
        .child(
            text(move || row.get().kind.glyph())
                .text_color(move || kind_color(row.get().kind))
                .bg(bg.clone()),
        )
        .child(
            text(id)
                .flex_shrink_0()
                .text_color(theme().muted_foreground)
                .bg(bg.clone()),
        )
        .child(
            text(due)
                .flex_shrink_0()
                .text_color(theme().muted_foreground)
                .bg(bg.clone()),
        )
        .child(
            text(who)
                .flex_shrink_0()
                .text_color(fg.clone())
                .bg(bg.clone()),
        )
        .child(
            text(text_line)
                .flex_1()
                .overflow_hidden()
                .text_color(fg)
                .bg(bg),
        )
}

fn editor(shared: Shared) -> AnyElement {
    let edit = match super::lock(&shared).mode.clone() {
        Mode::Edit(edit) => *edit,
        _ => return view().into_element(),
    };
    let label = |name: &'static str| text(name).text_color(theme().muted_foreground);

    let change = |set: fn(&mut super::state::Edit, String)| {
        let store = shared.clone();
        move |value: &str| {
            if let Some(edit) = super::lock(&store).edit_mut() {
                set(edit, value.to_owned());
                edit.dirty = true;
            }
            bump(&revision_signal());
        }
    };
    let on_text = change(|edit, value| edit.text = value);
    let on_session = change(|edit, value| edit.session = value);
    let on_when = change(|edit, value| edit.when = value);

    // The branch is rebuilt each time the form is entered, so the first
    // field is focused once its node exists. Tab then travels with the
    // engine; the `on_focus` handlers only keep the hint in step.
    let refs = (element_ref(), element_ref(), element_ref());
    focus_soon(refs.0, 20);
    let track = |focus: Field| {
        let store = shared.clone();
        move || {
            if let Some(edit) = super::lock(&store).edit_mut() {
                edit.focus = focus;
            }
            bump(&revision_signal());
        }
    };

    let field_hint = {
        let store = shared.clone();
        move || {
            let _ = rev();
            let field = store
                .lock()
                .unwrap()
                .mode
                .clone()
                .into_edit()
                .map(|e| e.focus);
            match field {
                Some(Field::Text) => "prompt",
                Some(Field::Session) => "session",
                Some(Field::When) => "due",
                None => "prompt",
            }
        }
    };
    view()
        .flex_col()
        .gap(px(1.0))
        .child(
            text(format!("editing {} — ctrl+s saves, esc backs out", edit.id))
                .text_color(theme().primary),
        )
        .child(label("prompt"))
        .child(
            input()
                .element_ref(&refs.0)
                .value(field_value(shared.clone(), |edit| edit.text.clone()))
                .multiline(true)
                .h(px(7.0))
                .on_focus(track(Field::Text))
                .on_change(on_text),
        )
        .child(label("session"))
        .child(
            input()
                .element_ref(&refs.1)
                .value(field_value(shared.clone(), |edit| edit.session.clone()))
                .h(px(1.0))
                .on_focus(track(Field::Session))
                .on_change(on_session),
        )
        .child(label("due"))
        .child(
            input()
                .element_ref(&refs.2)
                .value(field_value(shared.clone(), |edit| edit.when.clone()))
                .h(px(1.0))
                .on_focus(track(Field::When))
                .on_change(on_when),
        )
        .child(
            text(move || format!("field: {} · ctrl+p picks a session", field_hint()))
                .text_color(theme().muted_foreground),
        )
        .into_element()
}

fn picker(shared: Shared) -> impl IntoElement {
    let loading = {
        let store = shared.clone();
        move || {
            let _ = rev();
            let store = super::lock(&store);
            store.picking && store.sessions.is_empty()
        }
    };
    let empty = {
        let store = shared.clone();
        move || {
            let _ = rev();
            super::lock(&store).sessions.is_empty()
        }
    };
    view()
        .flex_col()
        .child(when(loading, || {
            text("loading sessions…").text_color(theme().muted_foreground)
        }))
        .child(when(empty.clone(), || {
            view()
                .flex_col()
                .child(text("no sessions on the server").text_color(theme().destructive))
                .child(text("esc keeps the current id").text_color(theme().muted_foreground))
        }))
        .child(view().flex_col().child(when(empty, {
            let store = shared.clone();
            move || sessions_list(store.clone())
        })))
}

fn sessions_list(shared: Shared) -> impl IntoElement {
    each(
        {
            let store = shared.clone();
            move || {
                let _ = rev();
                super::lock(&store).sessions.clone()
            }
        },
        |(id, _)| id.clone(),
        {
            let store = shared.clone();
            move |item: ReadSignal<(String, String)>, _at: ReadSignal<usize>| {
                let selected = {
                    let store = store.clone();
                    move || {
                        let _ = rev();
                        let store = super::lock(&store);
                        store
                            .sessions
                            .get(store.picker_selected)
                            .is_some_and(|(id, _)| *id == item.get().0)
                    }
                };
                let bg = {
                    let selected = selected.clone();
                    move || {
                        if selected() {
                            theme().accent
                        } else {
                            theme().background
                        }
                    }
                };
                let fg = {
                    let selected = selected.clone();
                    move || {
                        if selected() {
                            theme().accent_foreground
                        } else {
                            theme().foreground
                        }
                    }
                };
                view()
                    .flex_row()
                    .bg(bg)
                    .child(
                        text(move || format!(" {} ", item.get().0))
                            .text_color(theme().muted_foreground),
                    )
                    .child(text(move || item.get().1.clone()).text_color(fg))
            }
        },
    )
}

fn status(shared: Shared) -> impl IntoElement {
    let hint = {
        let store = shared.clone();
        move || {
            let _ = rev();
            let mode = super::lock(&store).mode.clone();
            match mode {
                Mode::Error(_) => "r retry · q quit",
                Mode::Help => "? esc back",
                Mode::Confirm(_) => "y confirm · n back",
                Mode::Edit(_) => "ctrl+s save · tab next · esc back",
                Mode::Filter => "type to filter · enter keeps · esc clears",
                Mode::Picker => "j k move · enter pick · esc back",
                Mode::Browse => {
                    "e edit · d cancel · f fire now · r requeue · y copy id · / filter · ? keys · q quit"
                }
            }
        }
    };
    let line = {
        let store = shared.clone();
        move || {
            let _ = rev();
            let hint = hint();
            let store = super::lock(&store);
            let confirm = match &store.mode {
                Mode::Confirm(c) => Some(c.label.clone()),
                _ => None,
            };
            let notice = store.notice.clone().map(|n| (n.text, n.bad));
            match (confirm, notice) {
                (Some(label), _) => (label, true),
                (None, Some((text, bad))) => (text, bad),
                (None, None) => (hint.to_owned(), false),
            }
        }
    };
    let hint = line.clone();
    view()
        .flex_col()
        .h(px(2.0))
        .px(px(1.0))
        .bg(theme().secondary)
        .child(text(move || line().0).text_color(move || {
            if hint().1 {
                nsh_core::color::rgb(WARN).into()
            } else {
                theme().muted_foreground
            }
        }))
}

fn help() -> impl IntoElement {
    let line = |keys: &str, what: &str| {
        view()
            .flex_row()
            .child(text(format!("  {keys:<16}")).text_color(theme().primary))
            .child(text(what.to_owned()).text_color(theme().foreground))
    };
    view()
        .flex_col()
        .gap(px(1.0))
        .child(text("keys").text_color(theme().muted_foreground))
        .child(line("j ↓ / k ↑", "move"))
        .child(line("g / G", "first / last"))
        .child(line("pgdn / pgup", "page"))
        .child(line("e enter", "edit text, session and time"))
        .child(line("d del", "cancel a pending prompt"))
        .child(line("f", "fire now"))
        .child(line("r", "requeue as a new prompt"))
        .child(line("y", "copy the job id"))
        .child(line("/", "filter"))
        .child(line("q", "quit"))
        .child(
            text("editing: ctrl+s saves, esc discards, ctrl+p picks a session")
                .text_color(theme().muted_foreground),
        )
}

fn error(shared: Shared) -> impl IntoElement {
    view()
        .flex_col()
        .gap(px(1.0))
        .child(
            text("the queue could not be read — nothing will be written")
                .text_color(theme().destructive),
        )
        .child(text(move || {
            let _ = rev();
            match super::lock(&shared).mode.clone() {
                Mode::Error(msg) => msg,
                _ => String::new(),
            }
        }))
}

/// A field's text, read from the store on every commit: the field writes
/// through `on_change`, so a plain value would put the stale text back.
fn field_value(shared: Shared, get: fn(&super::state::Edit) -> String) -> impl Fn() -> String {
    move || {
        let _ = rev();
        shared
            .lock()
            .unwrap()
            .mode
            .clone()
            .into_edit()
            .map(|edit| get(&edit))
            .unwrap_or_default()
    }
}

/// Focus a field once its node exists (the branch may still be mounting).
fn focus_soon(field: ElementRef, tries: u8) {
    trace!("[focus_soon] tries={tries} node={:?}", field.node());
    if field.node().is_some() {
        field.focus();
        return;
    }
    if tries == 0 {
        return;
    }
    set_timeout(Duration::from_millis(10), move || {
        focus_soon(field, tries - 1)
    });
}

/// Focus the root once its node exists, retrying for about a second.
fn try_focus(root: ElementRef, store: Shared, tries: u8) {
    trace!(
        "[fx] tries={tries} node={:?} focused={}",
        root.node(),
        super::lock(&store).focused
    );
    if super::lock(&store).focused || tries == 0 {
        return;
    }
    if root.node().is_some() {
        root.focus();
    }
    set_timeout(Duration::from_millis(50), move || {
        try_focus(root, store, tries - 1)
    });
}
