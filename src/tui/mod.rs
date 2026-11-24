// ---------------------------------------------------------------------------
// mod.rs — the TUI's terminal shell.
//
// Owns the alternate screen, raw mode, the input poll and the effects the
// screens ask for; `state.rs` decides, `ui.rs` draws.
// ---------------------------------------------------------------------------

mod input;
#[cfg(test)]
mod probe;
mod state;
mod ui;

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use nsh_base::registry as base_registry;
use nsh_core::element::*;
use nsh_platform_tui::{
    App, AppOptions, AppView, Input, Modifiers, MouseInput, MouseKind, Root, RunEvent,
};
use ratatui_core::backend::Backend;
use ratatui_core::layout::Size;
use ratatui_core::terminal::Terminal;

use mdrv_oc::schedule::{JobExpect, JobPatch, Queue, SessionSwap, now_ms, parse_due};

use state::{Action, Field, Key, Mode, Store};

type Shared = ui::Shared;

const TICK: Duration = Duration::from_millis(16);

/// Open the queue and run until the user leaves.
pub(crate) fn run(url: Option<String>) -> Result<()> {
    let mut guard = Guard::enter()?;

    let queue = match Queue::load_display() {
        Ok(queue) => queue,
        Err(e) => {
            // Still start: the error screen can retry, and nothing writes.
            let store = Store::new(Queue::default(), url.clone(), 20);
            let mut store = store;
            store.mode = Mode::Error(format!("{e}"));
            store.daemon = mdrv_oc::schedule::daemon_running();
            return drive(
                guard.terminal(),
                std::sync::Arc::new(std::sync::Mutex::new(store)),
            );
        }
    };
    let mut store = Store::new(queue, url, 20);
    store.daemon = mdrv_oc::schedule::daemon_running();
    if !store.daemon {
        store.set_notice(
            "the daemon is not running — due prompts fire only once it is",
            true,
        );
    }
    drive(
        guard.terminal(),
        std::sync::Arc::new(std::sync::Mutex::new(store)),
    )
}

/// The terminal, restored on drop, on panic, and on the way out.
struct Guard;

static ENTERED: AtomicBool = AtomicBool::new(false);

impl Guard {
    fn enter() -> Result<Self> {
        if !std::io::stdin().is_terminal() {
            bail!("the TUI reads keys from a terminal, and stdin is not one");
        }
        enable_raw_mode()?;
        let mut out = std::io::stdout();
        execute!(
            out,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture,
            Hide
        )?;
        ENTERED.store(true, Ordering::SeqCst);

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        Ok(Self)
    }

    fn terminal(&mut self) -> Terminal<impl Backend> {
        // The backend only needs the writer; restoring is ours, so errors on
        // drop are already covered by `restore`.
        Terminal::new(ratatui_crossterm::CrosstermBackend::new(std::io::stdout()))
            .expect("a terminal")
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

/// Leave the alternate screen exactly once, whatever path got here.
fn restore() {
    if !ENTERED.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = std::io::stdout();
    let _ = execute!(
        out,
        DisableMouseCapture,
        DisableBracketedPaste,
        Show,
        LeaveAlternateScreen
    );
    let _ = out.flush();
    let _ = disable_raw_mode();
}

/// Trace lines for working on the console itself: set `MDRV_OC_TUI_TRACE`.
macro_rules! trace {
    ($($arg:tt)*) => {
        if std::env::var_os("MDRV_OC_TUI_TRACE").is_some() {
            eprintln!($($arg)*);
        }
    };
}
pub(super) use trace;

fn drive(mut terminal: Terminal<impl Backend>, store: Shared) -> Result<()> {
    let mut options = AppOptions::default();
    options.ui_sets = vec![base_registry()];
    // The engine wakes the loop for timers, animations and async work; the
    // flag turns that into a redraw (the poll's own wait is short).
    let wake = std::sync::Arc::new(AtomicBool::new(false));
    let flag = wake.clone();
    options.wake = Some(std::sync::Arc::new(move || {
        flag.store(true, Ordering::SeqCst)
    }));
    let mut view = AppView::new(options);
    view.on_app_error(|e| trace!("[app error] {e:?}"));
    view.on_cursor_style_changed(|s| trace!("[cursor] {s:?}"));
    ui::set_store(store.clone());
    view.load(App::Rust(Root::of::<ui::App>()))?;
    // Keys are ignored by the surface until it is told it has focus, and the
    // focus itself lands only once the tree is mounted: draw and nudge until
    // the root reports it.
    view.set_focused(true);
    let mut focused = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    for boot_i in 0..1000 {
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .map_err(|e| anyhow!("could not draw a frame: {e}"))?;
        trace!("[boot] i={boot_i} size={:?} pre-draw", terminal.size());
        trace!("[boot] i={boot_i} pre-lock");
        if lock(&store).focused {
            focused = true;
            break;
        }
        trace!("[boot] i={boot_i} pre-sync");
        sync(&mut view);
        trace!("[boot] i={boot_i} pre-tab");
        view.set_focused(true);
        view.handle(nsh_platform_tui::Input::Key {
            key: "Tab".into(),
            modifiers: nsh_platform_tui::Modifiers::default(),
            release: false,
        });
        std::thread::sleep(Duration::from_millis(10));
        if std::time::Instant::now() > deadline {
            break;
        }
    }

    trace!("[boot] focused={focused}");
    let mut redraw = true;
    let mut settle = 0;
    let mut last_size = Size::default();

    // The engine owns the reactive graph, so every write happens inside it:
    // keys go through `view.handle`, and the loop's own events (a fetch
    // landing, a resize, the mouse) are stashed and pushed with a synthetic
    // key the root handles.
    let mut it: u64 = 0;
    loop {
        it += 1;
        if it.is_multiple_of(200) {
            trace!("[loop] {it}");
        }
        let size = terminal.size().unwrap_or_default();
        if size != last_size && size.height > 0 {
            last_size = size;
            stash_size(size);
            trace!("[sync] pre");
            sync(&mut view);
            trace!("[sync] post");
            redraw = true;
        }
        if redraw || true {
            trace!("[draw]");
            terminal
                .draw(|frame| view.render(frame, frame.area()))
                .map_err(|e| anyhow!("could not draw a frame: {e}"))?;
            redraw = false;
            trace!("[draw done] selected={}", lock(&store).selected);
            if settle > 0 {
                settle -= 1;
                sync(&mut view);
                std::thread::sleep(Duration::from_millis(4));
                redraw = true;
            }
        }
        if lock(&store).quit {
            break;
        }

        // A picker fetch finishing is the only thing that arrives off-loop.
        if picker_pending()
            && let Some(result) = picker_recv()
        {
            stash_picker(result);
            trace!("[sync] pre");
            sync(&mut view);
            trace!("[sync] post");
            redraw = true;
        }

        let wait = view
            .next_frame_deadline()
            .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()))
            .map_or(TICK, |left| left.min(TICK));
        match input::poll(wait.max(Duration::from_millis(1)), &wake)? {
            Some(RunEvent::Input(event)) => {
                if let Input::Mouse(mouse) = event {
                    stash_mouse(mouse);
                    trace!("[sync] pre");
                    sync(&mut view);
                    trace!("[sync] post");
                } else {
                    if let Input::Focus(focused) = event {
                        view.set_focused(focused);
                    }
                    let handled = view.handle(event);
                    trace!("[handled] {handled:?}");
                    // The engine applies a handler's writes after the event,
                    // so give it another turn before the frame is drawn.
                    sync(&mut view);
                    settle = 2;
                }
                redraw = true;
            }
            Some(RunEvent::Redraw) => {
                redraw = true;
                trace!("[sync] pre");
                sync(&mut view);
                trace!("[sync] post");
            }
            Some(RunEvent::Exit) => break,
            None | Some(RunEvent::Idle) | Some(_) => {}
        }
        guard_finished(&store);
    }
    view.stop();
    Ok(())
}

fn guard_finished(store: &Shared) {
    if lock(store).quit && ENTERED.load(Ordering::SeqCst) {
        // `drive` returns straight into `Guard::drop`; nothing to do yet.
    }
}

/// What the loop has for the engine, handed over with the next sync key.
#[derive(Default)]
struct Pending {
    size: Option<Size>,
    mouse: Option<MouseInput>,
    picker: Option<Result<Vec<(String, String)>>>,
}

/// The loop stashes on the main thread; the root drains inside the engine
/// thread, so this cannot be a thread local.
static PENDING: std::sync::Mutex<Pending> = std::sync::Mutex::new(Pending::new());

impl Pending {
    const fn new() -> Self {
        Self {
            size: None,
            mouse: None,
            picker: None,
        }
    }
}

pub(super) struct Lk<'a> {
    guard: std::sync::MutexGuard<'a, Store>,
    n: u64,
}

impl std::ops::Deref for Lk<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.guard
    }
}

impl std::ops::DerefMut for Lk<'_> {
    fn deref_mut(&mut self) -> &mut Store {
        &mut self.guard
    }
}

impl Drop for Lk<'_> {
    fn drop(&mut self) {
        trace!("[t{:?}] [unlock] {}", std::thread::current().id(), self.n);
    }
}

#[track_caller]
pub(super) fn lock(store: &Shared) -> Lk<'_> {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    trace!(
        "[t{:?}] [lock] {n} wait {}",
        std::thread::current().id(),
        std::panic::Location::caller()
    );
    let g = store.lock().unwrap_or_else(|p| p.into_inner());
    trace!("[t{:?}] [lock] {n} got", std::thread::current().id());
    Lk { guard: g, n }
}

fn stash_size(size: Size) {
    lock_pending().size = Some(size);
}

fn stash_mouse(mouse: MouseInput) {
    lock_pending().mouse = Some(mouse);
}

fn stash_picker(result: Result<Vec<(String, String)>>) {
    lock_pending().picker = Some(result);
}

fn lock_pending() -> std::sync::MutexGuard<'static, Pending> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

fn take_pending() -> Pending {
    std::mem::take(&mut *lock_pending())
}

/// Push stashed loop events into the engine and let the root handle them.
fn sync(view: &mut AppView) {
    view.handle(Input::Key {
        key: ui::SYNC.into(),
        modifiers: Modifiers::default(),
        release: false,
    });
}

/// Apply everything stashed, inside the engine where writes are allowed.
pub(super) fn drain(store: &Shared, rev: &Signal<u64>) {
    let pending = take_pending();
    trace!(
        "[drain] size={:?} mouse={} picker={}",
        pending.size,
        pending.mouse.is_some(),
        pending.picker.is_some()
    );
    let mut redraw = false;
    if let Some(size) = pending.size {
        let mut store = lock(store);
        store.height = usize::from(size.height.saturating_sub(4)).max(1);
        store.width = usize::from(size.width).max(20);
        store.clamp_offset();
        redraw = true;
    }
    if let Some(mouse) = pending.mouse {
        redraw |= handle_mouse(store, mouse);
    }
    if let Some(result) = pending.picker {
        let mut store = lock(store);
        match result {
            Ok(sessions) if sessions.is_empty() => {
                store.set_notice("the server has no sessions", true)
            }
            Ok(sessions) => {
                store.sessions = sessions;
                store.picker_selected = 0;
                store.mode = Mode::Picker;
            }
            Err(e) => store.set_notice(format!("{e:#}"), true),
        }
        redraw = true;
    }
    let _ = redraw;
    ui::bump(rev);
}

/// The key path shared by the engine poll and the root's own handler.
pub(super) fn dispatch(
    store: &Shared,
    rev: &Signal<u64>,
    event: &nsh_core::element::KeyEvent,
) -> bool {
    apply_key(store, rev, &event.key, event.modifiers)
}

/// A key from a shortcut (`on_key`): those fire where raw keys do not, while
/// a field has the focus.
pub(super) fn send(store: &Shared, rev: &Signal<u64>, key: Key) {
    trace!("[send] {key:?}");
    let action = state::on_key(&mut lock(store), key);
    trace!(
        "[send2] action={action:?} mode={:?}",
        std::mem::discriminant(&lock(store).mode)
    );
    if let Err(e) = perform(store, rev, action) {
        lock(store).set_notice(format!("{e:#}"), true);
    }
    ui::keep_focus(store);
    ui::bump(rev);
}

fn apply_key(store: &Shared, rev: &Signal<u64>, name: &str, modifiers: Modifiers) -> bool {
    let Some(key) = map_key(name, modifiers) else {
        return false;
    };
    // While a session fetch is in flight, escape drops it instead of leaving.
    if key == Key::Esc && lock(store).picking {
        picker_clear();
        let mut store = lock(store);
        store.picking = false;
        store.set_notice("session list dropped", false);
        return true;
    }
    // In the form the field owns typing, deletion, movement and Tab: those
    // keys are left to it (and to the engine's focus travel).
    let typed = matches!(
        key,
        Key::Char(_)
            if !modifiers.ctrl && !modifiers.alt
    ) || matches!(
        key,
        Key::Backspace
            | Key::Delete
            | Key::Home
            | Key::End
            | Key::Up
            | Key::Down
            | Key::Enter
            | Key::Tab
            | Key::BackTab
    );
    if typed && lock(store).mode.is_edit() {
        return false;
    }
    send(store, rev, key);
    ui::keep_focus(store);
    true
}

fn handle_mouse(store: &Shared, mouse: MouseInput) -> bool {
    {
        let mut store = lock(store);
        match mouse.kind {
            MouseKind::Wheel { y, .. } => {
                store.move_by(if y > 0 { 3 } else { -3 });
                true
            }
            MouseKind::Down(_) => {
                // Rows start at terminal row 2: the header, then one padding row.
                let row = usize::from(mouse.row).saturating_sub(2);
                let target = store.offset + row;
                if row < store.height && target < store.rows.len() {
                    store.selected = target;
                    store.clamp_offset();
                }
                true
            }
            _ => false,
        }
    }
}

/// Fields a key event carries that we care about.
fn map_key(name: &str, modifiers: Modifiers) -> Option<Key> {
    let ctrl = modifiers.ctrl;
    let plain = |c: char| {
        if ctrl {
            Some(Key::Ctrl(c.to_ascii_lowercase()))
        } else {
            Some(Key::Char(c))
        }
    };
    match name {
        "Enter" | "Return" => Some(Key::Enter),
        "Escape" => Some(Key::Esc),
        "Tab" if modifiers.shift => Some(Key::BackTab),
        "Tab" => Some(Key::Tab),
        "ArrowUp" => Some(Key::Up),
        "ArrowDown" => Some(Key::Down),
        "PageUp" => Some(Key::PageUp),
        "PageDown" => Some(Key::PageDown),
        "Home" => Some(Key::Home),
        "End" => Some(Key::End),
        "Backspace" => Some(Key::Backspace),
        "Delete" => Some(Key::Delete),
        _ => {
            if ctrl && name.len() == 1 {
                return plain(name.chars().next()?);
            }
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => plain(c),
                _ => None,
            }
        }
    }
}

fn perform(store: &Shared, _rev: &Signal<u64>, action: Action) -> Result<()> {
    match action {
        Action::Quit | Action::None | Action::Redraw => {}
        Action::Reload => reload(store)?,
        Action::SaveEdit => save_edit(store)?,
        Action::CancelJob => cancel_job(store)?,
        Action::FireNow => fire_now(store)?,
        Action::Requeue => requeue(store)?,
        Action::CopyId => copy_id(store)?,
        Action::PickSession => start_picker(store),
        Action::ApplyPicker => apply_picker(store)?,
    }
    Ok(())
}

fn reload(store: &Shared) -> Result<()> {
    let result = Queue::load_display();
    let mut store = lock(store);
    match result {
        Ok(queue) => {
            store.queue = queue;
            store.mode = Mode::Browse;
            store.refresh();
            store.daemon = mdrv_oc::schedule::daemon_running();
        }
        Err(e) => store.mode = Mode::Error(format!("{e}")),
    }
    Ok(())
}

/// Re-read the queue after a write, keeping the notice that says what changed.
fn settle(store: &Shared, notice: String, keep_edit: bool) -> Result<()> {
    let mut store = lock(store);
    // A re-sort must not walk the selection onto a neighbour.
    let picked = store.selected_job().map(|job| job.id.clone());
    store.daemon = mdrv_oc::schedule::daemon_running();
    match Queue::load_display() {
        Ok(queue) => {
            store.queue = queue;
            if !keep_edit {
                store.mode = Mode::Browse;
            }
            store.refresh();
            if let Some(id) = picked {
                store.select_id(&id);
                trace!("[settle] picked={id} selected={}", store.selected);
            }
        }
        Err(e) => {
            store.mode = Mode::Error(format!("{e}"));
        }
    }
    store.set_notice(notice, false);
    Ok(())
}

fn selected(store: &Shared) -> Result<(String, JobExpect)> {
    let store = lock(store);
    let job = store
        .selected_job()
        .ok_or_else(|| anyhow!("nothing selected"))?;
    Ok((job.id.clone(), JobExpect::of(job)))
}

fn save_edit(store: &Shared) -> Result<()> {
    if !matches!(&lock(store).mode, Mode::Edit(form) if form.dirty) {
        lock(store).leave_edit();
        return Ok(());
    }
    let (id, expect, patch, changed) = {
        let store = lock(store);
        let Mode::Edit(edit) = &store.mode else {
            return Ok(());
        };
        let due =
            parse_due(Some(&edit.when), None, &jiff::Zoned::now()).map_err(|e| anyhow!("{e}"))?;
        if edit.text.trim().is_empty() {
            bail!("the prompt text is empty");
        }
        let mut changed = Vec::new();
        if edit.text != edit.expect.text {
            changed.push("text");
        }
        if edit.session != edit.expect.session_id {
            changed.push("session");
        }
        if due != edit.expect.due_ms {
            changed.push("due");
        }
        let patch = JobPatch {
            text: (edit.text != edit.expect.text).then(|| edit.text.clone()),
            session: (edit.session != edit.expect.session_id).then(|| SessionSwap {
                id: edit.session.clone(),
                title: resolve_title(store.url.as_deref(), &edit.session),
            }),
            due_ms: (due != edit.expect.due_ms).then_some(due),
        };
        (edit.id.clone(), edit.expect.clone(), patch, changed)
    };
    if changed.is_empty() {
        lock(store).leave_edit();
        return Ok(());
    }
    let job = Queue::with_locked(|q| {
        let idx = q.find_job(&id)?;
        q.update_job(idx, &expect, &patch)
    })?;
    let late = patch.due_ms.is_some_and(|due| due <= now_ms());
    let mut notice = format!("updated {}: {}", job.id, changed.join(", "));
    if late {
        notice.push_str(" — due is in the past, it fires on the next tick");
    }
    settle(store, notice, false)
}

/// Best-effort title for a new session id; `None` clears the old one.
fn resolve_title(url: Option<&str>, id: &str) -> Option<String> {
    let ep = mdrv_oc::server::resolve_endpoint(url).ok()?;
    let sessions = mdrv_oc::server::sessions(&ep, 1000).ok()?;
    sessions
        .iter()
        .find(|s| s.id == id)
        .and_then(|s| s.title.clone())
}

fn cancel_job(store: &Shared) -> Result<()> {
    let (id, expect) = selected(store)?;
    let removed = Queue::with_locked(|q| {
        let idx = q.find_job(&id)?;
        q.cancel_job(idx, Some(&expect))
    })?;
    settle(store, format!("cancelled {}", removed.id), false)
}

fn fire_now(store: &Shared) -> Result<()> {
    let (id, expect) = selected(store)?;
    let job = Queue::with_locked(|q| {
        let idx = q.find_job(&id)?;
        q.fire_job_now(idx, &expect)
    })?;
    let notice = if lock(store).daemon {
        format!("{} is due now — the daemon fires it within seconds", job.id)
    } else {
        format!("{} is due now, but the daemon is not running", job.id)
    };
    settle(store, notice, false)
}

fn requeue(store: &Shared) -> Result<()> {
    let (id, _) = selected(store)?;
    let copy = Queue::with_locked(|q| {
        let idx = q.find_job(&id)?;
        q.requeue_job(idx, now_ms())
    })?;
    settle(
        store,
        format!(
            "requeued as {} — it is due now; `e` sets a later time",
            copy.id
        ),
        false,
    )?;
    // Select the copy and open its form, so the time can be typed at once.
    {
        let mut store = lock(store);
        if let Some(pos) = store
            .rows
            .iter()
            .position(|r| store.queue.jobs[r.index].id == copy.id)
        {
            store.selected = pos;
            store.clamp_offset();
        }
    }
    lock(store).enter_edit();
    Ok(())
}

fn copy_id(store: &Shared) -> Result<()> {
    let id = store
        .lock()
        .unwrap()
        .selected_job()
        .map(|job| job.id.clone())
        .ok_or_else(|| anyhow!("nothing selected"))?;
    osc52(&id)?;
    lock(store).set_notice(format!("copied {id}"), false);
    Ok(())
}

fn osc52(text: &str) -> Result<()> {
    let mut out = std::io::stdout();
    out.write_all(b"\x1b]52;c;")?;
    out.write_all(base64(text.as_bytes()).as_bytes())?;
    out.write_all(b"\x07")?;
    out.flush()?;
    Ok(())
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

type PickerRx = Receiver<Result<Vec<(String, String)>>>;

static PICKER: std::sync::Mutex<Option<PickerRx>> = std::sync::Mutex::new(None);

fn picker_pending() -> bool {
    lock_picker().is_some()
}

fn lock_picker() -> std::sync::MutexGuard<'static, Option<PickerRx>> {
    PICKER.lock().unwrap_or_else(|e| e.into_inner())
}

/// Take whatever the fetch thread sent, if anything. `None` means "keep waiting
/// (or the screen was already handled)"; a disconnect becomes an error.
fn picker_recv() -> Option<Result<Vec<(String, String)>>> {
    let mut slot = lock_picker();
    let rx = slot.as_ref()?;
    match rx.try_recv() {
        Ok(result) => {
            *slot = None;
            Some(result)
        }
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => {
            *slot = None;
            Some(Err(anyhow!("the session list fetch died")))
        }
    }
}

fn picker_clear() {
    *lock_picker() = None;
}

fn apply_picker(store: &Shared) -> Result<()> {
    let mut store = lock(store);
    let Some((id, _title)) = store.sessions.get(store.picker_selected).cloned() else {
        return Ok(());
    };
    store.mode = Mode::Browse;
    if let Some(edit) = store.edit_mut() {
        if edit.session != id {
            edit.dirty = true;
        }
        edit.session = id;
        edit.focus = Field::When;
    }
    Ok(())
}

fn start_picker(store: &Shared) {
    if lock(store).picking {
        return;
    }
    let url = lock(store).url.clone();
    lock(store).picking = true;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> Result<Vec<(String, String)>> {
            let ep = mdrv_oc::server::resolve_endpoint(url.as_deref())?;
            let sessions = mdrv_oc::server::sessions(&ep, 100)?;
            Ok(sessions
                .into_iter()
                .map(|s| (s.id, s.title.unwrap_or_else(|| "(untitled)".into())))
                .collect())
        })();
        let _ = tx.send(result);
    });
    *lock_picker() = Some(rx);
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_pads_like_the_spec() {
        assert_eq!(base64(b"sch_ab"), "c2NoX2Fi");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }
}
