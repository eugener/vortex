use super::*;

/// A base instant plus an offset in milliseconds, so a test reads as a timeline.
fn at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

#[test]
fn the_first_press_is_a_single_click() {
    let mut clicks = Clicks::new();
    assert_eq!(clicks.press(4, 2, Instant::now()), 1);
}

#[test]
fn presses_in_the_same_cell_count_up_then_start_over() {
    // 1, 2, 3, and then back to 1: a user who keeps clicking is trying to get
    // somewhere, and sticking on "line" would be a gesture with no way back.
    let base = Instant::now();
    let mut clicks = Clicks::new();
    assert_eq!(clicks.press(4, 2, base), 1);
    assert_eq!(clicks.press(4, 2, at(base, 100)), 2);
    assert_eq!(clicks.press(4, 2, at(base, 200)), 3);
    assert_eq!(clicks.press(4, 2, at(base, 300)), 1);
    assert_eq!(clicks.press(4, 2, at(base, 400)), 2);
}

#[test]
fn a_slow_second_press_is_a_fresh_click() {
    // Two deliberate clicks a second apart are two clicks, not a double.
    let base = Instant::now();
    let mut clicks = Clicks::new();
    assert_eq!(clicks.press(4, 2, base), 1);
    assert_eq!(clicks.press(4, 2, at(base, 1000)), 1);
}

#[test]
fn the_window_is_measured_from_the_previous_press_not_the_first() {
    // A slow but continuous run still counts up: each press restarts the clock, so
    // three clicks at 300ms apart are a triple even though they span 600ms.
    let base = Instant::now();
    let mut clicks = Clicks::new();
    assert_eq!(clicks.press(4, 2, base), 1);
    assert_eq!(clicks.press(4, 2, at(base, 300)), 2);
    assert_eq!(clicks.press(4, 2, at(base, 600)), 3);
}

#[test]
fn a_press_in_a_different_cell_starts_over() {
    // Aimed somewhere else is a different gesture, however fast it followed.
    let base = Instant::now();
    let mut clicks = Clicks::new();
    assert_eq!(clicks.press(4, 2, base), 1);
    assert_eq!(clicks.press(5, 2, at(base, 50)), 1, "one column over");
    assert_eq!(clicks.press(5, 3, at(base, 100)), 1, "one row down");
    assert_eq!(clicks.press(5, 3, at(base, 150)), 2, "and back on target");
}

#[test]
fn reset_ends_the_run() {
    // An Alt-click or a drag is not part of the sequence; without this it would
    // leave a run behind for an unrelated click to continue.
    let base = Instant::now();
    let mut clicks = Clicks::new();
    clicks.press(4, 2, base);
    clicks.reset();
    assert_eq!(clicks.press(4, 2, at(base, 50)), 1);
}

#[test]
fn a_press_exactly_at_the_window_edge_is_a_fresh_click() {
    // The boundary is exclusive, so the rule is "strictly within the window" -
    // stated here because it is the kind of thing a refactor silently flips.
    let base = Instant::now();
    let mut clicks = Clicks::new();
    clicks.press(4, 2, base);
    assert_eq!(clicks.press(4, 2, base + WINDOW), 1);
}
