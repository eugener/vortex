//! The overlay compositor - the frontend's UI layer stack (SPEC §7.5).
//!
//! §7 fixes how a frame reaches the terminal (own the loop, sync-output, let
//! ratatui cell-diff). This module is the layer *above* the renderer: a stack of
//! floating [`Layer`]s (prompt line, command palette, pickers, …) painted over the
//! base editor view and given first crack at input.
//!
//! This is Helix's compositor **minus the custom renderer** (SPEC §7.5 "job 2
//! only"): we do not replace ratatui's cell-diffing, we only manage which overlay
//! is on top, route keys to it, and paint the stack. ratatui supplies the paint
//! primitives (a layer punches a hole with the `Clear` widget, then draws); we
//! supply the ~one-file stack and event routing that ratatui has no notion of.
//!
//! Like [`crate::layout`], this is pure logic with no terminal I/O, so it is
//! unit-testable end to end against an in-memory [`Buffer`] and synthetic
//! [`KeyEvent`]s (SPEC §13) - the I/O shell in `main.rs` only feeds it.
//!
//! **Seam rule (SPEC §7.5):** navigating *inside* a layer never round-trips to the
//! core. A layer handles its own keys locally; only a *committed* intent (a picked
//! command, a submitted path) becomes an `Action`, which the layer emits via
//! [`Layer::take_actions`] and the compositor hands back to the event loop.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::{Position, Rect};
use vortex_core::Action;

/// What a [`Layer`] decides about an input event.
///
/// `Consumed` stops propagation - neither a lower layer nor the base editor sees
/// the key. `Ignored` offers it to the next layer down, and ultimately (when no
/// layer takes it) back to the editor's own keymap. This is the mechanism behind
/// the §7.5 seam rule: an overlay `Consumed`s its navigation keys so they stay
/// frontend-local instead of leaking to the core as editor actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventResult {
    /// The layer used the event; do not offer it to layers below or the editor.
    Consumed,
    /// The layer did not use it; offer it to the next layer down.
    Ignored,
}

/// One floating UI surface in the stack: prompt line, command palette, picker,
/// which-key popup, completion menu (SPEC §7.5 surface table).
///
/// A layer is handed the **full screen area** and positions itself within it (a
/// prompt docks to the bottom row, a palette centers a box), painting a `Clear`
/// over its own region first so the editor beneath does not bleed through. Keeping
/// placement in the layer keeps the [`Compositor`] trivial - it only orders and
/// routes, matching Helix's `Component`/`Compositor` split.
pub trait Layer {
    /// Paint this layer onto `screen`. Called bottom-to-top, so a later (higher)
    /// layer overwrites the cells of an earlier one where they overlap.
    fn render(&self, screen: Rect, buf: &mut Buffer);

    /// Handle a key. Returning [`EventResult::Consumed`] stops propagation; the
    /// layer may also mark itself [finished](Layer::is_finished) here (e.g. Enter
    /// submits, Esc cancels) so the compositor pops it after the dispatch.
    fn handle_key(&mut self, key: KeyEvent) -> EventResult;

    /// Drain any `Action`s the layer has committed (the §7.5 seam rule: only a
    /// committed intent crosses to the core). Polled by the compositor after each
    /// key it hands the layer, so a submit both closes the layer and emits its
    /// action in one dispatch. Default: emits nothing.
    fn take_actions(&mut self) -> Vec<Action> {
        Vec::new()
    }

    /// Where the terminal's real cursor should sit while this layer is on top, if
    /// anywhere (a text prompt wants it in its input field; a menu does not). Only
    /// the topmost layer's cursor is honored. Default: no cursor.
    fn cursor(&self, _screen: Rect) -> Option<Position> {
        None
    }

    /// Whether the layer has run its course and should be removed from the stack
    /// (a submitted prompt, a picked item, a cancelled overlay). Checked after each
    /// key dispatch. Default: never finishes on its own.
    fn is_finished(&self) -> bool {
        false
    }
}

/// The stack of overlay [`Layer`]s above the base editor view (SPEC §7.5).
///
/// Empty in the common case - the editor paints and handles keys directly. When a
/// surface is opened (a prompt, a palette) it is [pushed](Compositor::push) on top;
/// it then gets first refusal on every key until it finishes and is popped. The
/// editor is *not* itself a layer: it is the always-present base the compositor
/// renders over and falls through to, so the hot editing path pays nothing for the
/// stack when no overlay is open.
#[derive(Default)]
pub struct Compositor {
    /// Bottom-to-top: `layers[0]` is the deepest, the last element is the topmost
    /// (focused) layer that sees input first and renders last.
    layers: Vec<Box<dyn Layer>>,
}

impl Compositor {
    /// A compositor with no overlays - the editor owns the screen.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether no overlay is open. When true the event loop routes keys straight to
    /// the editor keymap and paints the editor's own cursor.
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Push a new overlay on top; it becomes the focused layer.
    pub fn push(&mut self, layer: Box<dyn Layer>) {
        self.layers.push(layer);
    }

    /// Offer a key to the stack, top-down, stopping at the first layer that
    /// [`EventResult::Consumed`]s it, and return that outcome plus any `Action`s the
    /// handling layers committed. On [`EventResult::Ignored`] the caller falls
    /// through to the editor's keymap; the returned actions are sent to the core.
    ///
    /// Every finished layer is then removed, order preserved. Normally that is just
    /// the top one a submit/cancel closed; but a lower layer that finished on a key
    /// the top *ignored* is pruned too, so no finished layer can leak beneath an open
    /// one. A committing layer's actions are collected *before* it is removed.
    pub fn handle_key(&mut self, key: KeyEvent) -> (EventResult, Vec<Action>) {
        let mut result = EventResult::Ignored;
        let mut actions = Vec::new();
        for layer in self.layers.iter_mut().rev() {
            let outcome = layer.handle_key(key);
            actions.append(&mut layer.take_actions());
            if outcome == EventResult::Consumed {
                result = EventResult::Consumed;
                break;
            }
        }
        self.layers.retain(|l| !l.is_finished());
        (result, actions)
    }

    /// Paint every layer over `screen`, bottom-to-top, so the topmost overlay wins
    /// any shared cell. The base editor is painted by the caller *before* this, so
    /// the overlays land on top of it.
    pub fn render(&self, screen: Rect, buf: &mut Buffer) {
        for layer in &self.layers {
            layer.render(screen, buf);
        }
    }

    /// The cursor position to show, taken from the topmost layer (an overlay owns
    /// the caret while it is focused). `None` when the stack is empty *or* the top
    /// layer wants no cursor, in which case the caller uses the editor's own caret.
    pub fn cursor(&self, screen: Rect) -> Option<Position> {
        self.layers.last().and_then(|l| l.cursor(screen))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A configurable test [`Layer`]: records (in a shared log) the order in which
    /// layers saw a key, optionally consumes it, optionally finishes on a given key,
    /// optionally emits an action once, and paints its `id` digit at a fixed cell so
    /// render order is visible.
    struct Fake {
        id: u8,
        consume: bool,
        finish_on: Option<KeyCode>,
        finished: bool,
        emit: Option<Action>,
        marker: Option<(u16, u16)>,
        cursor: Option<Position>,
        log: Rc<RefCell<Vec<u8>>>,
    }

    impl Fake {
        fn new(id: u8, consume: bool, log: &Rc<RefCell<Vec<u8>>>) -> Self {
            Self {
                id,
                consume,
                finish_on: None,
                finished: false,
                emit: None,
                marker: None,
                cursor: None,
                log: Rc::clone(log),
            }
        }
        fn finishing_on(mut self, code: KeyCode) -> Self {
            self.finish_on = Some(code);
            self
        }
        fn emitting(mut self, action: Action) -> Self {
            self.emit = Some(action);
            self
        }
        fn at(mut self, x: u16, y: u16) -> Self {
            self.marker = Some((x, y));
            self
        }
        fn with_cursor(mut self, pos: Position) -> Self {
            self.cursor = Some(pos);
            self
        }
    }

    impl Layer for Fake {
        fn render(&self, _screen: Rect, buf: &mut Buffer) {
            if let Some((x, y)) = self.marker {
                buf.set_string(x, y, self.id.to_string(), ratatui::style::Style::default());
            }
        }
        fn handle_key(&mut self, key: KeyEvent) -> EventResult {
            self.log.borrow_mut().push(self.id);
            if self.finish_on == Some(key.code) {
                self.finished = true;
            }
            if self.consume {
                EventResult::Consumed
            } else {
                EventResult::Ignored
            }
        }
        fn take_actions(&mut self) -> Vec<Action> {
            self.emit.take().into_iter().collect()
        }
        fn cursor(&self, _screen: Rect) -> Option<Position> {
            self.cursor
        }
        fn is_finished(&self) -> bool {
            self.finished
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn boxed(f: Fake) -> Box<dyn Layer> {
        Box::new(f)
    }

    #[test]
    fn empty_compositor_ignores_keys_and_shows_no_cursor() {
        let mut c = Compositor::new();
        let screen = Rect::new(0, 0, 10, 4);
        assert!(c.is_empty());
        // No overlay: the key falls through (Ignored) so the editor keymap runs.
        let (res, actions) = c.handle_key(key('a'));
        assert_eq!(res, EventResult::Ignored);
        assert!(actions.is_empty());
        assert_eq!(c.cursor(screen), None);
        // Rendering an empty stack touches nothing.
        let mut buf = Buffer::empty(screen);
        c.render(screen, &mut buf);
        assert_eq!(buf, Buffer::empty(screen));
    }

    #[test]
    fn top_layer_consuming_stops_propagation() {
        // A modal overlay consumes its keys so they never reach the editor.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut c = Compositor::new();
        c.push(boxed(Fake::new(1, true, &log)));
        assert!(!c.is_empty());
        assert_eq!(c.handle_key(key('x')).0, EventResult::Consumed);
        assert_eq!(*log.borrow(), vec![1]);
    }

    #[test]
    fn key_is_offered_top_down_until_consumed() {
        // Stack: layer 1 (bottom, consumes), layer 2 (top, ignores). The top sees
        // the key first; because it ignores, the bottom layer then consumes it.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut c = Compositor::new();
        c.push(boxed(Fake::new(1, true, &log))); // bottom, consumes
        c.push(boxed(Fake::new(2, false, &log))); // top, ignores
        assert_eq!(c.handle_key(key('k')).0, EventResult::Consumed);
        // Order proves top-down offering: layer 2 saw it before layer 1.
        assert_eq!(*log.borrow(), vec![2, 1]);
    }

    #[test]
    fn key_ignored_by_every_layer_falls_through() {
        // No layer consumes -> the compositor reports Ignored so the editor runs.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut c = Compositor::new();
        c.push(boxed(Fake::new(1, false, &log)));
        c.push(boxed(Fake::new(2, false, &log)));
        assert_eq!(c.handle_key(key('z')).0, EventResult::Ignored);
        assert_eq!(*log.borrow(), vec![2, 1]);
    }

    #[test]
    fn committed_actions_flow_back_from_the_handling_layer() {
        // A layer that emits an action on the key it consumes: the compositor hands
        // that action back to the loop (which forwards it to the core).
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut c = Compositor::new();
        c.push(boxed(
            Fake::new(1, true, &log).emitting(Action::RequestSnapshot),
        ));
        let (res, actions) = c.handle_key(key('x'));
        assert_eq!(res, EventResult::Consumed);
        assert!(matches!(actions.as_slice(), [Action::RequestSnapshot]));
    }

    #[test]
    fn finished_top_layer_is_popped_after_dispatch() {
        // A layer that finishes on Esc (submit/cancel) is removed once it handles
        // that key, returning the screen to the editor.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut c = Compositor::new();
        c.push(boxed(Fake::new(1, true, &log).finishing_on(KeyCode::Esc)));
        assert!(!c.is_empty());
        assert_eq!(c.handle_key(esc()).0, EventResult::Consumed);
        assert!(c.is_empty(), "finished layer should be popped");
    }

    #[test]
    fn only_the_finished_top_layer_is_popped() {
        // Two stacked layers; only the top finishes. The lower one survives, so a
        // nested overlay (palette over a prompt) closes one level at a time.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut c = Compositor::new();
        c.push(boxed(Fake::new(1, false, &log))); // bottom, never finishes
        c.push(boxed(Fake::new(2, true, &log).finishing_on(KeyCode::Esc))); // top
        assert_eq!(c.handle_key(esc()).0, EventResult::Consumed);
        assert!(!c.is_empty(), "the lower layer must remain");
        // The surviving layer still receives the next key.
        assert_eq!(c.handle_key(key('a')).0, EventResult::Ignored);
        assert_eq!(*log.borrow(), vec![2, 1]);
    }

    #[test]
    fn a_finished_layer_below_an_open_one_is_pruned() {
        // The top layer ignores the key so it propagates; the bottom consumes it and
        // finishes. Removal is order-preserving over the whole stack, not top-only,
        // so the finished bottom layer does not leak beneath the still-open top one.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut c = Compositor::new();
        c.push(boxed(Fake::new(1, true, &log).finishing_on(KeyCode::Esc))); // bottom
        c.push(boxed(Fake::new(2, false, &log))); // top, ignores
        assert_eq!(c.handle_key(esc()).0, EventResult::Consumed);
        assert!(!c.is_empty(), "the open top layer remains");
        // Only layer 2 is left: the next key reaches it, never the pruned layer 1.
        log.borrow_mut().clear();
        c.handle_key(key('a'));
        assert_eq!(*log.borrow(), vec![2]);
    }

    #[test]
    fn layers_render_bottom_to_top() {
        // Two layers mark the same cell; the top (last pushed) must win.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut c = Compositor::new();
        c.push(boxed(Fake::new(1, false, &log).at(2, 1)));
        c.push(boxed(Fake::new(2, false, &log).at(2, 1)));
        let screen = Rect::new(0, 0, 10, 4);
        let mut buf = Buffer::empty(screen);
        c.render(screen, &mut buf);
        assert_eq!(buf.cell((2, 1)).unwrap().symbol(), "2");
    }

    #[test]
    fn cursor_comes_from_the_top_layer() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut c = Compositor::new();
        let screen = Rect::new(0, 0, 10, 4);
        c.push(boxed(
            Fake::new(1, false, &log).with_cursor(Position::new(1, 1)),
        ));
        c.push(boxed(
            Fake::new(2, false, &log).with_cursor(Position::new(5, 3)),
        ));
        assert_eq!(c.cursor(screen), Some(Position::new(5, 3)));
    }

    #[test]
    fn top_layer_without_a_cursor_hides_it() {
        // A menu-style top layer (no cursor) hides the caret even though a lower
        // layer would have shown one - the focused layer decides.
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut c = Compositor::new();
        let screen = Rect::new(0, 0, 10, 4);
        c.push(boxed(
            Fake::new(1, false, &log).with_cursor(Position::new(1, 1)),
        ));
        c.push(boxed(Fake::new(2, false, &log))); // top, no cursor
        assert_eq!(c.cursor(screen), None);
    }
}
