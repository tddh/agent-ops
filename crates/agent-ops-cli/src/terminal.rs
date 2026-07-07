use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter_raw_mode() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}
