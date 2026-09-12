//! Interactive session entry: Crossterm terminal setup and the keyboard loop.
//!
//! The loop is deliberately thin — it snapshots the read model, draws, and
//! feeds keys to [`crate::tui::handle_key`]. No workflow effect happens here:
//! the surface is an observer, and every control it understands is a local
//! selection/focus change or quit.

use std::io;

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use super::board;
use super::{Action, ColorMode, ReadModel, UiState, handle_key};

/// Run the operator surface against `model` until the operator quits.
///
/// Sets up the terminal (alternate screen, raw mode) through Ratatui's
/// Crossterm integration and always restores it afterwards, including on
/// error paths. Keyboard-only controls are documented on
/// [`crate::tui::handle_key`].
pub fn run<M: ReadModel>(model: &M) -> io::Result<()> {
    run_with_mode(model, ColorMode::detect())
}

/// [`run`] with an explicit colour mode instead of auto-detection.
///
/// The seam exists for embedders and deterministic environments; rendering
/// itself always takes the mode as a parameter.
pub fn run_with_mode<M: ReadModel>(model: &M, mode: ColorMode) -> io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let mut state = UiState::default();
    let session = (|| -> io::Result<()> {
        loop {
            let view = model.snapshot();
            terminal.draw(|frame| board::draw(&view, &state, mode, frame))?;
            let event = event::read()?;
            if let Event::Key(key) = event
                && key.kind == KeyEventKind::Press
                && matches!(handle_key(&mut state, &view, key), Some(Action::Quit))
            {
                break;
            }
        }
        Ok(())
    })();
    ratatui::restore();
    session
}
