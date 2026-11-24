//! Rendered checks for the console: the real `App` in front of a test
//! backend, so bindings, focus and layout are exercised, not just the state.
//!
//! Everything runs in one test because the surface store is a process-wide
//! `OnceLock`.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ratatui_core::backend::TestBackend;
use ratatui_core::terminal::Terminal;

use super::state::{Edit, Field, Mode, Store};
use super::{lock, trace, ui};

type Shared = Arc<Mutex<Store>>;

fn job(id: &str, text: &str, state: mdrv_oc::schedule::JobState) -> mdrv_oc::schedule::Job {
    mdrv_oc::schedule::Job {
        id: id.into(),
        created_ms: 1,
        due_ms: mdrv_oc::schedule::now_ms() + 3_600_000,
        session_id: "ses_ff67b028".into(),
        session_title: None,
        text: text.into(),
        agent: None,
        model: None,
        state,
        attempts: 0,
        last_attempt_ms: None,
        last_error: None,
        done_ms: None,
    }
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    let mut out = String::new();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            out.push_str(buffer.cell((x, y)).map_or(" ", |c| c.symbol()));
        }
        out.push('\n');
    }
    out
}

/// Draws until `wanted` shows up, so a frame that lands a turn late is fine.
fn until(
    terminal: &mut Terminal<TestBackend>,
    view: &mut nsh_platform_tui::AppView,
    wanted: &str,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let text = screen(terminal);
        if text.contains(wanted) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "never showed {wanted:?}:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn press(view: &mut nsh_platform_tui::AppView, key: &str) {
    view.handle(nsh_platform_tui::Input::Key {
        key: key.into(),
        modifiers: nsh_platform_tui::Modifiers::default(),
        release: false,
    });
}

fn open_edit(store: &Shared) {
    let mut store = lock(store);
    let row = store.selected_row().expect("a selected row");
    let job = store.job(row).clone();
    store.mode = Mode::Edit(Box::new(Edit {
        id: job.id.clone(),
        expect: mdrv_oc::schedule::JobExpect::of(&job),
        text: job.text.clone(),
        session: job.session_id.clone(),
        when: mdrv_oc::schedule::format_ms(job.due_ms),
        focus: Field::Text,
        dirty: false,
    }));
}

#[test]
fn the_console_renders_and_reacts() {
    use mdrv_oc::schedule::{JobState, Queue};

    let mut queue = Queue::default();
    queue
        .jobs
        .push(job("sch_aaaa1111", "first prompt", JobState::Pending));
    queue
        .jobs
        .push(job("sch_bbbb2222", "second prompt", JobState::Pending));
    queue
        .jobs
        .push(job("sch_cccc3333", "third prompt", JobState::Failed));
    let store: Shared = Arc::new(Mutex::new(Store::new(queue, None, 20)));
    lock(&store).refresh();

    let mut options = nsh_platform_tui::AppOptions::default();
    options.ui_sets = vec![nsh_base::registry()];
    options.cache = nsh_platform_tui::CacheOptions::off();
    let mut view = nsh_platform_tui::AppView::new(options);
    view.on_app_error(|e| trace!("app error: {e:?}"));
    ui::set_store(store.clone());
    view.load(nsh_platform_tui::App::Rust(nsh_platform_tui::Root::of::<
        ui::App,
    >()))
    .unwrap();
    view.set_focused(true);

    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    // The root is only focusable once it is mounted, and nothing focuses it
    // by itself: nudge with `tab` until the focus lands.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !lock(&store).focused {
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        press(&mut view, "Tab");
        std::thread::sleep(Duration::from_millis(10));
        assert!(Instant::now() < deadline, "the root never took the focus");
    }

    // The selected row is marked with a caret, not only with a colour.
    let text = until(&mut terminal, &mut view, "first prompt");
    assert!(text.contains("▸ sch_aaaa1111"), "{text}");
    press(&mut view, "ArrowDown");
    let text = until(&mut terminal, &mut view, "▸ sch_bbbb2222");
    assert!(!text.contains("▸ sch_aaaa1111"), "{text}");

    // A re-sort keeps the caret on the job, not on the line.
    {
        let mut store = lock(&store);
        store.queue.jobs.swap(0, 1);
        store.refresh();
        store.select_id("sch_bbbb2222");
    }
    press(&mut view, "F24"); // the sync key bumps inside the engine
    let text = until(&mut terminal, &mut view, "second prompt");
    let marked = text
        .lines()
        .find(|line| line.contains('▸'))
        .unwrap_or_default()
        .to_owned();
    assert!(marked.contains("sch_bbbb2222"), "{text}");

    // Leaving a dirty form asks first, and `n` brings it back.
    open_edit(&store);
    press(&mut view, "F24"); // the sync key bumps inside the engine
    let text = until(&mut terminal, &mut view, "field: prompt");
    assert!(text.contains("second prompt"), "{text}");
    lock(&store).edit_mut().unwrap().dirty = true;
    press(&mut view, "Escape");
    let text = until(&mut terminal, &mut view, "discard unsaved changes?");
    assert!(text.contains("second prompt"), "{text}");
    press(&mut view, "n");
    let text = until(&mut terminal, &mut view, "field: prompt");
    assert!(!text.contains("discard unsaved changes?"), "{text}");

    // Below the minimum size the list is swapped for a message.
    {
        let mut store = lock(&store);
        store.mode = Mode::Browse;
        store.width = 40;
        store.height = 6;
    }
    press(&mut view, "F24"); // the sync key bumps inside the engine
    let text = until(&mut terminal, &mut view, "terminal too small");
    assert!(text.contains("need 64x12 · this is 40x10"), "{text}");

    view.stop();
}
