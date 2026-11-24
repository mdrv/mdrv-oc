// ---------------------------------------------------------------------------
// state.rs — everything the TUI decides without touching a terminal.
//
// The screen, the key map and the row window live here so they can be unit
// tested; `mod.rs` performs the effects (queue writes, clipboard, reload) and
// `ui.rs` only draws what this module says.
// ---------------------------------------------------------------------------

use mdrv_oc::schedule::{Job, JobExpect, Queue};

use super::trace;

/// How many finished jobs the list shows; the pending ones always show.
pub const RECENT_CAP: usize = 20;

/// The job states as the list cares about them. A `delivering` value is only
/// past its freshness window stale, hence the separate kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Pending,
    Delivering,
    Delivered,
    Failed,
}

impl Kind {
    pub fn of(job: &Job, now_ms: i64) -> Kind {
        if job.in_flight(now_ms) {
            return Kind::Delivering;
        }
        match job.state {
            mdrv_oc::schedule::JobState::Pending | mdrv_oc::schedule::JobState::Delivering => {
                Kind::Pending
            }
            mdrv_oc::schedule::JobState::Delivered => Kind::Delivered,
            mdrv_oc::schedule::JobState::Failed => Kind::Failed,
        }
    }

    /// A glyph carries the state on its own, so colour is never the only cue.
    pub fn glyph(self) -> &'static str {
        match self {
            Kind::Pending => "·",
            Kind::Delivering => "⏳",
            Kind::Delivered => "✓",
            Kind::Failed => "✗",
        }
    }

    pub fn is_finished(self) -> bool {
        matches!(self, Kind::Delivered | Kind::Failed)
    }
}

/// One rendered line: the queue index plus the state kind resolved once, so
/// drawing never re-derives (and never disagrees with the key map).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub index: usize,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Text,
    Session,
    When,
}

/// The job being edited, plus the snapshot its save is guarded by.
#[derive(Debug, Clone)]
pub struct Edit {
    pub id: String,
    pub expect: JobExpect,
    pub text: String,
    pub session: String,
    pub when: String,
    pub focus: Field,
    pub dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    CancelJob,
    DiscardEdit,
    QuitDuringEdit,
}

#[derive(Debug, Clone)]
pub struct ConfirmState {
    pub what: Confirm,
    pub label: String,
    /// The form waiting behind the question, restored on "no".
    pub edit: Option<Box<Edit>>,
}

#[derive(Debug, Clone)]
pub enum Mode {
    Browse,
    Filter,
    Edit(Box<Edit>),
    Help,
    Confirm(ConfirmState),
    /// Choosing a session id from the server's list.
    Picker,
    /// The queue could not be read; writes are off until it loads again.
    Error(String),
}

impl Mode {
    pub fn is_edit(&self) -> bool {
        matches!(self, Mode::Edit(_))
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Mode::Error(_))
    }

    pub fn into_edit(self) -> Option<Edit> {
        match self {
            Mode::Edit(edit) => Some(*edit),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Reload,
    SaveEdit,
    CancelJob,
    FireNow,
    Requeue,
    CopyId,
    PickSession,
    /// Take the highlighted picker entry.
    ApplyPicker,
    /// The text changed and the engine should redraw.
    Redraw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub bad: bool,
}

#[derive(Debug, Clone)]
pub struct Store {
    pub queue: Queue,
    pub rows: Vec<Row>,
    pub mode: Mode,
    pub filter: String,
    pub selected: usize,
    /// The first row on screen; kept so a selection move only scrolls when it
    /// leaves the window.
    pub offset: usize,
    /// Rows the viewport can show, minus the keys that scroll it.
    pub height: usize,
    pub width: usize,
    pub daemon: bool,
    /// The root view holds the terminal's keyboard focus.
    pub focused: bool,
    /// Focus attempts since start, to bound the boot retry.
    pub url: Option<String>,
    pub notice: Option<Notice>,
    pub quit: bool,
    /// A server session list is being fetched for the picker.
    pub picking: bool,
    pub sessions: Vec<(String, String)>,
    pub picker_selected: usize,
}

impl Store {
    pub fn new(queue: Queue, url: Option<String>, height: usize) -> Self {
        let mut store = Self {
            queue,
            rows: Vec::new(),
            mode: Mode::Browse,
            filter: String::new(),
            selected: 0,
            offset: 0,
            height: height.max(4),
            width: 80,
            daemon: false,
            focused: false,
            url,
            notice: None,
            quit: false,
            picking: false,
            sessions: Vec::new(),
            picker_selected: 0,
        };
        store.refresh();
        store
    }

    pub fn job(&self, row: Row) -> &Job {
        &self.queue.jobs[row.index]
    }

    pub fn selected_row(&self) -> Option<Row> {
        self.rows.get(self.selected).copied()
    }

    pub fn selected_job(&self) -> Option<&Job> {
        self.selected_row().map(|row| self.job(row))
    }

    /// Puts the selection back on a job after the rows were rebuilt, so a
    /// re-sort (fire now, a save) does not move it to a neighbour.
    pub fn select_id(&mut self, id: &str) {
        if let Some(at) = self.rows.iter().position(|row| self.job(*row).id == id) {
            self.selected = at;
            self.clamp_offset();
        }
    }

    /// Whether writes are possible at all right now.
    pub fn writable(&self) -> bool {
        !matches!(self.mode, Mode::Error(_))
    }

    pub fn set_notice(&mut self, text: impl Into<String>, bad: bool) {
        self.notice = Some(Notice {
            text: text.into(),
            bad,
        });
    }

    /// Rebuild the visible order: pending by due time, then the newest
    /// finished ones. The filter narrows all of it.
    pub fn refresh(&mut self) {
        let now = mdrv_oc::schedule::now_ms();
        let needle = self.filter.to_lowercase();
        let matches = |job: &Job| {
            needle.is_empty()
                || job.text.to_lowercase().contains(&needle)
                || job.id.to_lowercase().contains(&needle)
                || job.session_id.to_lowercase().contains(&needle)
                || job
                    .session_title
                    .as_deref()
                    .is_some_and(|t| t.to_lowercase().contains(&needle))
        };
        let kinded: Vec<(usize, Kind)> = self
            .queue
            .jobs
            .iter()
            .enumerate()
            .map(|(i, job)| (i, Kind::of(job, now)))
            .filter(|(_, kind)| !kind.is_finished())
            .collect();
        let mut live: Vec<(usize, Kind)> = kinded
            .into_iter()
            .filter(|(i, _)| matches(&self.queue.jobs[*i]))
            .collect();
        live.sort_by_key(|(i, _)| self.queue.jobs[*i].due_ms);

        let mut done: Vec<(i64, usize, Kind)> = self
            .queue
            .jobs
            .iter()
            .enumerate()
            .map(|(i, job)| (job.done_ms.unwrap_or(job.created_ms), i, Kind::of(job, now)))
            .filter(|(_, i, kind)| kind.is_finished() && matches(&self.queue.jobs[*i]))
            .collect();
        done.sort_by(|a, b| b.cmp(a));
        done.truncate(RECENT_CAP);

        self.rows = live
            .into_iter()
            .chain(done.into_iter().map(|(_, i, k)| (i, k)))
            .map(|(index, kind)| Row { index, kind })
            .collect();
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.clamp_offset();
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let target = self.selected.saturating_add_signed(delta).min(last);
        self.selected = target;
        self.clamp_offset();
    }

    pub fn go(&mut self, to_end: bool) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = if to_end { self.rows.len() - 1 } else { 0 };
        self.clamp_offset();
    }

    /// Keep the selected row inside the window, moving it by the least it can.
    pub fn clamp_offset(&mut self) {
        let height = self.height.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        self.offset = self.offset.min(self.rows.len().saturating_sub(height));
    }

    /// The rows the viewport shows, with the absolute index of the first.
    pub fn window(&self) -> (usize, &[Row]) {
        let start = self.offset.min(self.rows.len());
        let end = (start + self.height.max(1)).min(self.rows.len());
        (start, &self.rows[start..end])
    }

    pub fn enter_edit(&mut self) -> Action {
        let Some(job) = self.selected_job() else {
            return Action::None;
        };
        if self.selected_row().is_some_and(|r| r.kind.is_finished()) {
            self.set_notice("finished prompt — `r` requeues it", true);
            return Action::Redraw;
        }
        if self
            .selected_row()
            .is_some_and(|r| r.kind == Kind::Delivering)
        {
            self.set_notice("being delivered right now — try again in a moment", true);
            return Action::Redraw;
        }
        let edit = Edit {
            id: job.id.clone(),
            expect: JobExpect::of(job),
            text: job.text.clone(),
            session: job.session_id.clone(),
            when: mdrv_oc::schedule::format_ms(job.due_ms),
            focus: Field::Text,
            dirty: false,
        };
        self.mode = Mode::Edit(Box::new(edit));
        Action::Redraw
    }

    pub fn edit_mut(&mut self) -> Option<&mut Edit> {
        match &mut self.mode {
            Mode::Edit(edit) => Some(edit),
            _ => None,
        }
    }

    pub fn leave_edit(&mut self) {
        self.mode = Mode::Browse;
    }
}

// -- the key map ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Backspace,
    Delete,
    Ctrl(char),
}

/// Decide what a key does. Only state transitions happen here; I/O comes back
/// as an [`Action`] for the caller to run.
pub fn on_key(store: &mut Store, key: Key) -> Action {
    // A notice is transient: any key clears it.
    store.notice = None;

    match store.mode.clone() {
        Mode::Error(_) => match key {
            Key::Char('q') => Action::Quit,
            Key::Char('r') | Key::Enter => Action::Reload,
            _ => Action::None,
        },
        Mode::Help => match key {
            Key::Char('q') | Key::Esc | Key::Char('?') => {
                store.mode = Mode::Browse;
                Action::Redraw
            }
            _ => Action::None,
        },
        Mode::Confirm(state) => match key {
            Key::Char('y') | Key::Char('Y') => match state.what {
                Confirm::CancelJob => {
                    store.mode = Mode::Browse;
                    Action::CancelJob
                }
                Confirm::DiscardEdit => {
                    store.leave_edit();
                    Action::Redraw
                }
                Confirm::QuitDuringEdit => {
                    store.quit = true;
                    Action::Quit
                }
            },
            Key::Char('n') | Key::Char('N') | Key::Esc => {
                store.mode = match state.what {
                    Confirm::CancelJob => Mode::Browse,
                    Confirm::DiscardEdit | Confirm::QuitDuringEdit => {
                        state.edit.map_or(Mode::Browse, Mode::Edit)
                    }
                };
                Action::Redraw
            }
            _ => Action::None,
        },
        Mode::Filter => match key {
            Key::Esc => {
                store.filter.clear();
                store.mode = Mode::Browse;
                store.refresh();
                Action::Redraw
            }
            Key::Enter => {
                store.mode = Mode::Browse;
                Action::Redraw
            }
            Key::Up => {
                store.move_by(-1);
                Action::Redraw
            }
            Key::Down => {
                store.move_by(1);
                Action::Redraw
            }
            Key::Backspace => {
                store.filter.pop();
                store.refresh();
                Action::Redraw
            }
            Key::Char(c) => {
                store.filter.push(c);
                store.refresh();
                Action::Redraw
            }
            _ => Action::None,
        },
        Mode::Picker => match key {
            Key::Esc => {
                store.mode = Mode::Browse;
                Action::Redraw
            }
            Key::Char('j') | Key::Down => {
                let last = store.sessions.len().saturating_sub(1);
                store.picker_selected = (store.picker_selected + 1).min(last);
                Action::Redraw
            }
            Key::Char('k') | Key::Up => {
                store.picker_selected = store.picker_selected.saturating_sub(1);
                Action::Redraw
            }
            Key::Enter => Action::ApplyPicker,
            _ => Action::None,
        },
        Mode::Edit(_) => on_edit_key(store, key),
        Mode::Browse => match key {
            Key::Char('q') => Action::Quit,
            Key::Char('?') => {
                store.mode = Mode::Help;
                Action::Redraw
            }
            Key::Char('/') => {
                store.mode = Mode::Filter;
                Action::Redraw
            }
            Key::Char('j') | Key::Down => {
                store.move_by(1);
                Action::Redraw
            }
            Key::Char('k') | Key::Up => {
                store.move_by(-1);
                Action::Redraw
            }
            Key::PageDown | Key::Ctrl('d') => {
                store.move_by(store.height as isize);
                Action::Redraw
            }
            Key::PageUp | Key::Ctrl('u') => {
                store.move_by(-(store.height as isize));
                Action::Redraw
            }
            Key::Char('g') | Key::Home => {
                store.go(false);
                Action::Redraw
            }
            Key::Char('G') | Key::End => {
                store.go(true);
                Action::Redraw
            }
            Key::Char('e') | Key::Enter => store.enter_edit(),
            Key::Char('d') | Key::Delete => {
                let Some(row) = store.selected_row() else {
                    return Action::None;
                };
                if row.kind.is_finished() {
                    store.set_notice("already finished — `r` requeues it", true);
                    return Action::Redraw;
                }
                let job = store.job(row);
                store.mode = Mode::Confirm(ConfirmState {
                    what: Confirm::CancelJob,
                    label: format!("cancel {} ({})?", job.id, first_line(&job.text, 40)),
                    edit: None,
                });
                Action::Redraw
            }
            Key::Char('f') => Action::FireNow,
            Key::Char('r') => Action::Requeue,
            Key::Char('y') => Action::CopyId,
            _ => Action::None,
        },
    }
}

fn on_edit_key(store: &mut Store, key: Key) -> Action {
    let dirty = store.edit_mut().is_some_and(|e| e.dirty);
    trace!("[edit_key] {key:?} dirty={dirty}");
    // Take the form out of the mode while a question hangs over it, so "no"
    // can put it back untouched.
    let take_edit = |store: &mut Store| match std::mem::replace(&mut store.mode, Mode::Browse) {
        Mode::Edit(edit) => Some(edit),
        other => {
            store.mode = other;
            None
        }
    };
    if key == Key::Esc || (key == Key::Char('q') && dirty) {
        if dirty {
            let edit = take_edit(store);
            store.mode = Mode::Confirm(ConfirmState {
                what: if key == Key::Esc {
                    Confirm::DiscardEdit
                } else {
                    Confirm::QuitDuringEdit
                },
                label: if key == Key::Esc {
                    "discard unsaved changes?".to_owned()
                } else {
                    "quit with unsaved changes?".to_owned()
                },
                edit,
            });
            return Action::Redraw;
        }
        if key == Key::Char('q') {
            store.quit = true;
            return Action::Quit;
        }
        store.leave_edit();
        return Action::Redraw;
    }
    if key == Key::Ctrl('s') {
        return Action::SaveEdit;
    }
    let Some(edit) = store.edit_mut() else {
        return Action::None;
    };
    match key {
        Key::Tab => {
            edit.focus = match edit.focus {
                Field::Text => Field::Session,
                Field::Session => Field::When,
                Field::When => Field::Text,
            };
            Action::Redraw
        }
        Key::BackTab => {
            edit.focus = match edit.focus {
                Field::Text => Field::When,
                Field::Session => Field::Text,
                Field::When => Field::Session,
            };
            Action::Redraw
        }
        Key::Ctrl('p') => Action::PickSession,
        _ => Action::None,
    }
}

/// First line of a prompt, cut to `max` characters, for tight places.
pub fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    // The ellipsis has to fit inside `max`, so a truncated line spends one
    // column on it; the column budget from the row is exact.
    let max = max.max(8);
    let long = line.chars().count() > max;
    let keep = if long { max - 1 } else { max };
    let mut out: String = line.chars().take(keep).collect();
    if long {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdrv_oc::schedule::{Job, JobState};
    use pretty_assertions::assert_eq;

    fn at(id: &str, state: JobState, due: i64) -> Job {
        Job {
            id: id.into(),
            created_ms: due,
            due_ms: due,
            session_id: "ses_1".into(),
            session_title: None,
            agent: None,
            model: None,
            text: format!("text {id}"),
            state,
            attempts: 0,
            last_attempt_ms: None,
            last_error: None,
            done_ms: (state == JobState::Delivered).then_some(due),
        }
    }

    fn store_with(jobs: Vec<Job>) -> Store {
        Store::new(Queue { jobs }, None, 6)
    }

    #[test]
    fn pending_comes_first_and_finished_is_newest_first_and_capped() {
        let mut jobs = vec![
            at("sch_late", JobState::Pending, 300),
            at("sch_soon", JobState::Pending, 100),
            at("sch_old", JobState::Delivered, 50),
        ];
        for i in 0..RECENT_CAP + 5 {
            jobs.push(at(
                &format!("sch_d{i}"),
                JobState::Delivered,
                1000 + i as i64,
            ));
        }
        let store = store_with(jobs);
        let ids: Vec<&str> = store
            .rows
            .iter()
            .map(|r| store.queue.jobs[r.index].id.as_str())
            .collect();
        assert_eq!(&ids[..2], &["sch_soon", "sch_late"]);
        assert_eq!(ids.len(), 2 + RECENT_CAP);
        assert_eq!(ids[2], format!("sch_d{}", RECENT_CAP + 4));
    }

    #[test]
    fn the_filter_narrows_everything_it_touches() {
        let mut a = at("sch_a", JobState::Pending, 1);
        a.text = "deploy the site".into();
        let b = at("sch_b", JobState::Pending, 2);
        let mut store = store_with(vec![a, b]);
        store.filter = "DEPLOY".into();
        store.refresh();
        assert_eq!(store.rows.len(), 1);
        assert_eq!(store.queue.jobs[store.rows[0].index].id, "sch_a");
    }

    #[test]
    fn the_window_follows_the_selection_by_one_row() {
        let jobs = (0..20)
            .map(|i| at(&format!("sch_{i}"), JobState::Pending, i))
            .collect();
        let mut store = store_with(jobs);
        assert_eq!(store.window().0, 0);
        for _ in 0..5 {
            store.move_by(1);
        }
        // Still inside the 6-row window.
        assert_eq!(store.window().0, 0);
        store.move_by(1);
        assert_eq!(store.window().0, 1);
        store.go(true);
        assert_eq!(store.window().0, 14);
        store.go(false);
        assert_eq!(store.window().0, 0);
    }

    #[test]
    fn a_selection_survives_a_shrinking_list() {
        let jobs = vec![
            at("sch_a", JobState::Pending, 1),
            at("sch_b", JobState::Pending, 2),
        ];
        let mut store = store_with(jobs);
        store.go(true);
        store.filter = "sch_a".into();
        store.refresh();
        assert_eq!(store.selected, 0);
        assert_eq!(store.rows.len(), 1);
    }

    #[test]
    fn browse_keys_reach_the_actions_and_guard_finished_rows() {
        let store_jobs = vec![
            at("sch_a", JobState::Pending, 1),
            at("sch_d", JobState::Delivered, 2),
        ];
        let mut store = store_with(store_jobs);
        assert_eq!(on_key(&mut store, Key::Char('f')), Action::FireNow);
        assert_eq!(on_key(&mut store, Key::Char('e')), Action::Redraw);
        assert!(matches!(store.mode, Mode::Edit(_)));
        on_key(&mut store, Key::Esc);
        assert!(matches!(store.mode, Mode::Browse));

        store.go(true);
        assert_eq!(on_key(&mut store, Key::Char('e')), Action::Redraw);
        assert!(matches!(store.mode, Mode::Browse));
        assert!(
            store
                .notice
                .as_ref()
                .is_some_and(|n| n.text.contains("requeue"))
        );
    }

    #[test]
    fn cancel_asks_first_and_quits_with_unsaved_text() {
        let mut store = store_with(vec![at("sch_a", JobState::Pending, 1)]);
        on_key(&mut store, Key::Char('d'));
        assert!(matches!(
            store.mode,
            Mode::Confirm(ConfirmState {
                what: Confirm::CancelJob,
                ..
            })
        ));
        assert_eq!(on_key(&mut store, Key::Char('y')), Action::CancelJob);

        on_key(&mut store, Key::Char('e'));
        if let Some(edit) = store.edit_mut() {
            edit.dirty = true;
        }
        on_key(&mut store, Key::Esc);
        assert!(matches!(
            store.mode,
            Mode::Confirm(ConfirmState {
                what: Confirm::DiscardEdit,
                ..
            })
        ));
        on_key(&mut store, Key::Char('y'));
        assert!(matches!(store.mode, Mode::Browse));
    }

    #[test]
    fn the_filter_mode_takes_chars_and_restores_on_escape() {
        let mut store = store_with(vec![at("sch_a", JobState::Pending, 1)]);
        on_key(&mut store, Key::Char('/'));
        on_key(&mut store, Key::Char('t'));
        on_key(&mut store, Key::Char('e'));
        assert_eq!(store.filter, "te");
        on_key(&mut store, Key::Esc);
        assert_eq!(store.filter, "");
        assert!(matches!(store.mode, Mode::Browse));
    }

    #[test]
    fn an_error_screen_only_reloads_or_quits() {
        let mut store = store_with(vec![]);
        store.mode = Mode::Error("broken".into());
        assert_eq!(on_key(&mut store, Key::Char('e')), Action::None);
        assert_eq!(on_key(&mut store, Key::Char('r')), Action::Reload);
        assert!(!store.writable());
    }

    #[test]
    fn a_truncated_line_spends_a_column_on_the_ellipsis() {
        assert_eq!(first_line("run the full suite", 11), "run the fu…");
        assert_eq!(first_line("short", 11), "short");
        // The column budget can be tiny; never return more than asked for.
        assert!(first_line("run the full suite", 1).chars().count() <= 8);
    }
}
