//! Crossterm events to `nsh` input, and the blocking poll the loop sits on.
//!
//! `nsh` keeps its terminal shell in `nsh-tui-host`, which is a binary-first
//! crate loading a bundle; the console embeds `AppView` instead, so it needs
//! this small translation of its own. Key names follow the DOM (`ArrowLeft`,
//! `Escape`), the same spelling `nsh` uses internally.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use nsh_platform_tui::{Input, Modifiers, MouseButton, MouseInput, MouseKind, RunEvent};

/// Waits for input, but never longer than a slice, so a wake from the engine
/// (a timer, a fetch landing, an animation frame) can cut the wait short.
pub(super) fn poll(wait: Duration, wake: &AtomicBool) -> Result<Option<RunEvent>> {
    const SLICE: Duration = Duration::from_millis(5);
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        // Input comes first: a constantly woken loop must not starve the
        // keyboard, so the wake flag is only read when no key is waiting.
        if crossterm::event::poll(left.min(SLICE))? {
            let event = crossterm::event::read()?;
            return Ok(convert(event));
        }
        if wake.swap(false, Ordering::SeqCst) {
            return Ok(Some(RunEvent::Redraw));
        }
    }
}

fn convert(event: crossterm::event::Event) -> Option<RunEvent> {
    use crossterm::event::{Event, KeyEventKind, MouseEventKind as M};

    Some(match event {
        Event::Resize(..) => RunEvent::Redraw,
        Event::FocusGained => RunEvent::Input(Input::Focus(true)),
        Event::FocusLost => RunEvent::Input(Input::Focus(false)),
        Event::Paste(text) => RunEvent::Input(Input::Paste(text)),
        Event::Mouse(mouse) => {
            let kind = match mouse.kind {
                M::Down(button) => MouseKind::Down(button_of(button)),
                M::Up(button) => MouseKind::Up(button_of(button)),
                M::Drag(_) | M::Moved => MouseKind::Move,
                M::ScrollUp => MouseKind::Wheel { x: 0, y: -1 },
                M::ScrollDown => MouseKind::Wheel { x: 0, y: 1 },
                M::ScrollLeft => MouseKind::Wheel { x: -1, y: 0 },
                M::ScrollRight => MouseKind::Wheel { x: 1, y: 0 },
            };
            let mut input = MouseInput::default();
            input.kind = kind;
            input.column = mouse.column;
            input.row = mouse.row;
            input.modifiers = modifiers_of(mouse.modifiers, false);
            RunEvent::Input(Input::Mouse(input))
        }
        Event::Key(key) => {
            let name = key_name(key.code);
            let modifiers = modifiers_of(
                key.modifiers,
                key.code == crossterm::event::KeyCode::BackTab,
            );
            let input = if key.kind == KeyEventKind::Repeat {
                Input::KeyRepeat {
                    key: name,
                    modifiers,
                }
            } else {
                Input::Key {
                    key: name,
                    modifiers,
                    release: key.kind == KeyEventKind::Release,
                }
            };
            RunEvent::Input(input)
        }
    })
}

fn button_of(button: crossterm::event::MouseButton) -> MouseButton {
    use crossterm::event::MouseButton as B;
    match button {
        B::Left => MouseButton::Left,
        B::Right => MouseButton::Right,
        B::Middle => MouseButton::Middle,
    }
}

fn modifiers_of(modifiers: crossterm::event::KeyModifiers, backtab: bool) -> Modifiers {
    use crossterm::event::KeyModifiers as K;
    Modifiers {
        ctrl: modifiers.contains(K::CONTROL),
        alt: modifiers.contains(K::ALT),
        // A shifted Tab arrives as `BackTab`; the key itself stays `Tab`.
        shift: modifiers.contains(K::SHIFT) || backtab,
        cmd: modifiers.contains(K::SUPER),
    }
}

fn key_name(code: crossterm::event::KeyCode) -> String {
    use crossterm::event::KeyCode as K;
    match code {
        K::Char(c) => c.to_string(),
        K::Enter => "Enter".into(),
        K::Esc => "Escape".into(),
        K::Tab | K::BackTab => "Tab".into(),
        K::Backspace => "Backspace".into(),
        K::Delete => "Delete".into(),
        K::Insert => "Insert".into(),
        K::Home => "Home".into(),
        K::End => "End".into(),
        K::PageUp => "PageUp".into(),
        K::PageDown => "PageDown".into(),
        K::Left => "ArrowLeft".into(),
        K::Right => "ArrowRight".into(),
        K::Up => "ArrowUp".into(),
        K::Down => "ArrowDown".into(),
        K::F(number) => format!("F{number}"),
        K::CapsLock => "CapsLock".into(),
        K::ScrollLock => "ScrollLock".into(),
        K::NumLock => "NumLock".into(),
        K::PrintScreen => "PrintScreen".into(),
        K::Pause => "Pause".into(),
        K::Menu => "ContextMenu".into(),
        K::Null | K::KeypadBegin => "Unidentified".into(),
        K::Media(_) | K::Modifier(_) => "Unidentified".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    fn press(code: KeyCode, held: KeyModifiers) -> Option<RunEvent> {
        convert(Event::Key(KeyEvent::new(code, held)))
    }

    #[test]
    fn keys_keep_their_identity_and_modifiers() {
        let Some(RunEvent::Input(Input::Key {
            key,
            modifiers,
            release,
        })) = press(KeyCode::Char('d'), KeyModifiers::CONTROL)
        else {
            panic!("expected a key");
        };
        assert_eq!(key, "d");
        assert!(modifiers.ctrl);
        assert!(!release);

        let Some(RunEvent::Input(Input::Key { key, modifiers, .. })) =
            press(KeyCode::BackTab, KeyModifiers::NONE)
        else {
            panic!("expected a key");
        };
        assert_eq!(key, "Tab");
        assert!(modifiers.shift);

        let Some(RunEvent::Input(Input::Key { key, .. })) =
            press(KeyCode::Down, KeyModifiers::NONE)
        else {
            panic!("expected a key");
        };
        assert_eq!(key, "ArrowDown");
    }

    #[test]
    fn resize_is_a_redraw_and_the_wheel_keeps_its_sign() {
        assert!(matches!(
            convert(Event::Resize(80, 24)),
            Some(RunEvent::Redraw)
        ));
        let mouse = Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollDown,
            column: 5,
            row: 7,
            modifiers: KeyModifiers::NONE,
        });
        let Some(RunEvent::Input(Input::Mouse(mouse))) = convert(mouse) else {
            panic!("expected a mouse event");
        };
        assert_eq!((mouse.column, mouse.row), (5, 7));
        assert!(matches!(mouse.kind, MouseKind::Wheel { y: 1, .. }));
    }
}
