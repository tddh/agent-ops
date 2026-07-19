use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Debug)]
pub enum Action {
    ForwardToPty(()),
    AskQuestion,
    ClearHistory,
    Detach,
    Noop,
}

pub fn classify(key: &KeyEvent) -> Action {
    if key.kind != KeyEventKind::Press {
        return Action::Noop;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('g') if ctrl => Action::AskQuestion,
        KeyCode::Char('l') if ctrl => Action::ClearHistory,
        KeyCode::Char('\\') if ctrl => Action::Detach,
        KeyCode::Char('\x1c') => Action::Detach,
        _ => Action::ForwardToPty(()),
    }
}
