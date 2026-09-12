//! Interactive session entry: Crossterm terminal setup and the keyboard loop.
//!
//! The loops are deliberately thin. [`run`] snapshots the read model, draws,
//! and feeds keys to [`crate::tui::handle_key`]: no workflow effect happens
//! there — the surface is an observer, and every control it understands is a
//! local selection/focus change or quit. [`run_operator`] feeds keys to
//! [`OperatorConsole::handle_key`] instead, which owns the same board screen
//! plus the authority screens (preview, explicit authorization, outcome); the
//! loop itself stays as thin, and the console owns every effect boundary.

use std::io;

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use super::board;
use super::operator::{self, OperatorConsole};
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

/// Run the operator console (issue #91): the recorded board plus the exact
/// preview, the explicit authorization and the daemon-owned outcome.
///
/// Sets up and always restores the terminal exactly like [`run`]; the
/// difference is which key handler the loop feeds, and therefore where the
/// effect boundary lives (see [`OperatorConsole::handle_key`]).
pub fn run_operator(console: &mut OperatorConsole<'_>) -> io::Result<()> {
    run_operator_with_mode(console, ColorMode::detect())
}

/// [`run_operator`] with an explicit colour mode instead of auto-detection.
pub fn run_operator_with_mode(
    console: &mut OperatorConsole<'_>,
    mode: ColorMode,
) -> io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let session = (|| -> io::Result<()> {
        loop {
            terminal.draw(|frame| operator::draw(console, mode, frame))?;
            let event = event::read()?;
            // A resize clears the authorization box (the approved
            // interaction contract): an approval never survives a frame the
            // operator did not see.
            if let Event::Resize(_, _) = &event {
                console.on_resize();
            }
            if let Event::Key(key) = event
                && key.kind == KeyEventKind::Press
                && matches!(console.handle_key(key), Some(Action::Quit))
            {
                break;
            }
        }
        Ok(())
    })();
    ratatui::restore();
    session
}
