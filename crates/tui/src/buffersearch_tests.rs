use super::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use vortex_core::{Buffer as _, RopeBuffer};

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn type_str(layer: &mut dyn Layer, s: &str) {
    for c in s.chars() {
        layer.handle_key(key(c));
    }
}

fn text(s: &str) -> Text {
    RopeBuffer::from(s).text()
}

/// A left press at a screen cell.
fn click(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn find_prompt(replacing: bool) -> Find {
    Find::new(&Theme::default(), String::new(), replacing)
}

// --- Query ----------------------------------------------------------------

#[test]
fn a_query_matches_through_the_cores_own_engine() {
    // A preview that disagreed with the commit about what matches would be worse
    // than no preview, which is why this never builds a regex of its own.
    let t = text("cat dog cat\n");
    let q = Query::new("cat".into(), String::new());
    assert_eq!(q.matches_in(&t, 0..1), vec![0..3, 8..11]);
    assert_eq!(q.next_from(&t, 4), Some(8..11));
}

#[test]
fn a_half_typed_pattern_is_a_quiet_absence_not_an_error() {
    // Typing `(\w+)` passes through `(`, `(\`, `(\w`: an uncompilable pattern is the
    // normal state mid-keystroke, so it highlights nothing rather than complaining.
    let t = text("anything\n");
    for partial in ["(", r"(\", r"(\w", "[a-"] {
        let q = Query::new(partial.into(), String::new());
        assert!(q.matches_in(&t, 0..1).is_empty(), "{partial}");
        assert_eq!(q.next_from(&t, 0), None, "{partial}");
    }
}

#[test]
fn an_empty_pattern_matches_nothing() {
    // Opening the prompt must not light up the whole file before a key is pressed.
    let t = text("some text\n");
    let q = Query::new(String::new(), String::new());
    assert!(q.matches_in(&t, 0..1).is_empty());
}

// --- SearchState ----------------------------------------------------------

#[test]
fn refreshing_previews_the_next_match_from_where_the_search_began() {
    // Refining a query keeps finding the match nearest where the user was, rather
    // than walking away from it one keystroke at a time.
    let mut state = SearchState::default();
    state.begin(0);
    let t = text("cat dog cat");
    state.refresh(Query::new("cat".into(), String::new()), Some(&t));
    assert_eq!(state.preview(), Some(0..3));
    // A longer pattern still measures from the origin, not from the last preview.
    state.refresh(Query::new("dog".into(), String::new()), Some(&t));
    assert_eq!(state.preview(), Some(4..7));
}

#[test]
fn a_search_begun_mid_file_previews_from_there_and_wraps() {
    let mut state = SearchState::default();
    let t = text("hit\nhit\nhit");
    state.begin(5);
    state.refresh(Query::new("hit".into(), String::new()), Some(&t));
    assert_eq!(state.preview(), Some(8..11), "the next one down");
    // From past the last match it comes back around rather than showing nothing.
    state.begin(11);
    state.refresh(Query::new("hit".into(), String::new()), Some(&t));
    assert_eq!(state.preview(), Some(0..3));
}

#[test]
fn committing_ends_the_preview_but_keeps_the_pattern() {
    // The highlights stay up after Enter so find-next has something to walk, and the
    // pattern stays so it has something to repeat.
    let mut state = SearchState::default();
    let t = text("cat");
    state.begin(0);
    state.refresh(Query::new("cat".into(), String::new()), Some(&t));
    state.commit();
    assert_eq!(state.preview(), None, "the caret is on it now");
    assert!(state.query().is_some(), "the highlights stay");
    assert_eq!(
        state.repeat(false),
        Some(Action::SelectNextMatch {
            pattern: "cat".into(),
            backward: false,
        })
    );
}

#[test]
fn clearing_forgets_the_search_entirely() {
    let mut state = SearchState::default();
    let t = text("cat");
    state.begin(0);
    state.refresh(Query::new("cat".into(), String::new()), Some(&t));
    state.clear();
    assert!(state.query().is_none());
    assert_eq!(state.preview(), None);
    assert_eq!(state.repeat(false), None, "nothing left to repeat");
}

#[test]
fn repeating_before_any_search_is_a_no_op_rather_than_a_round_trip() {
    let state = SearchState::default();
    assert_eq!(state.repeat(false), None);
    assert_eq!(state.repeat(true), None);
    // ...and so is repeating an empty pattern, which would otherwise match nowhere
    // and cost a notification per keypress.
    let mut typed = SearchState::default();
    typed.refresh(Query::new(String::new(), String::new()), None);
    assert_eq!(typed.repeat(false), None);
}

#[test]
fn a_refresh_with_no_snapshot_yet_keeps_the_query_without_previewing() {
    let mut state = SearchState::default();
    state.refresh(Query::new("cat".into(), String::new()), None);
    assert_eq!(state.preview(), None);
    assert!(state.query().is_some());
}

// --- Find prompt ----------------------------------------------------------

#[test]
fn every_keystroke_publishes_the_query_so_the_preview_is_live() {
    let mut p = find_prompt(false);
    type_str(&mut p, "ca");
    assert_eq!(
        p.take_commands(),
        vec![
            Command::PreviewSearch {
                pattern: "c".into(),
                replacement: String::new(),
            },
            Command::PreviewSearch {
                pattern: "ca".into(),
                replacement: String::new(),
            },
        ],
        "one per key, not one on commit"
    );
}

#[test]
fn backspace_publishes_too_so_the_highlights_shrink_back() {
    let mut p = find_prompt(false);
    type_str(&mut p, "ca");
    p.take_commands();
    p.handle_key(press(KeyCode::Backspace));
    assert_eq!(
        p.take_commands(),
        vec![Command::PreviewSearch {
            pattern: "c".into(),
            replacement: String::new(),
        }]
    );
}

#[test]
fn enter_commits_a_find_and_closes() {
    let mut p = find_prompt(false);
    type_str(&mut p, "cat");
    p.take_commands();
    assert_eq!(p.handle_key(press(KeyCode::Enter)), EventResult::Consumed);
    assert!(p.is_finished());
    assert_eq!(p.take_commands(), vec![Command::FindNext]);
}

#[test]
fn escape_takes_the_highlights_down_with_it() {
    // The one gesture that means "done searching" - unlike a commit, which keeps the
    // query so find-next has a pattern.
    let mut p = find_prompt(false);
    type_str(&mut p, "cat");
    p.take_commands();
    p.handle_key(press(KeyCode::Esc));
    assert!(p.is_finished());
    assert_eq!(p.take_commands(), vec![Command::ClearSearch]);
}

#[test]
fn the_prompt_reopens_on_the_last_pattern() {
    // Refining a search you just ran should not mean retyping it.
    let mut p = Find::new(&Theme::default(), "previous".into(), false);
    p.handle_key(press(KeyCode::Enter));
    assert_eq!(p.take_commands(), vec![Command::FindNext]);
    assert_eq!(p.pattern, "previous");
}

#[test]
fn tab_moves_between_the_two_fields_of_a_replace() {
    let mut p = find_prompt(true);
    type_str(&mut p, "cat");
    p.handle_key(press(KeyCode::Tab));
    type_str(&mut p, "dog");
    assert_eq!(p.pattern, "cat");
    assert_eq!(p.replacement, "dog");
    // ...and back again, so a typo in the pattern is fixable without starting over.
    p.handle_key(press(KeyCode::Tab));
    type_str(&mut p, "s");
    assert_eq!(p.pattern, "cats");
}

#[test]
fn tab_in_a_plain_find_is_swallowed_rather_than_typed() {
    // There is nowhere to go, and a tab in the pattern would be a character the user
    // cannot see.
    let mut p = find_prompt(false);
    type_str(&mut p, "cat");
    p.handle_key(press(KeyCode::Tab));
    assert_eq!(p.pattern, "cat");
}

#[test]
fn a_replace_commits_into_the_query_replace_walk() {
    let mut p = find_prompt(true);
    type_str(&mut p, "cat");
    p.handle_key(press(KeyCode::Tab));
    type_str(&mut p, "dog");
    p.take_commands();
    p.handle_key(press(KeyCode::Enter));
    assert!(p.is_finished());
    assert_eq!(p.take_commands(), vec![Command::StartReplace]);
}

#[test]
fn a_ctrl_chord_is_deferred_not_typed() {
    // A shortcut is not input: the keymap stays the single source of bindings, the
    // same rule `Prompt` and the pickers follow.
    let mut p = find_prompt(false);
    let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(p.handle_key(ctrl_s), EventResult::Ignored);
    assert!(p.pattern.is_empty());
    assert!(!p.is_finished());
}

#[test]
fn clicking_away_cancels_the_search_and_clicking_the_row_does_not() {
    let screen = Rect::new(0, 0, 40, 10);
    let mut p = find_prompt(false);
    type_str(&mut p, "cat");
    p.take_commands();
    assert_eq!(p.handle_mouse(click(5, 9), screen), EventResult::Consumed);
    assert!(!p.is_finished(), "its own row is where the typing happens");

    assert_eq!(p.handle_mouse(click(5, 0), screen), EventResult::Consumed);
    assert!(p.is_finished());
    assert_eq!(p.take_commands(), vec![Command::ClearSearch]);
}

#[test]
fn the_prompt_paints_both_of_its_fields() {
    // The count answers "is this pattern finding anything", which is the question a
    // live search is actually asking.
    let mut p = find_prompt(true);
    type_str(&mut p, "cat");
    p.handle_key(press(KeyCode::Tab));
    type_str(&mut p, "dog");
    let mut terminal = Terminal::new(TestBackend::new(60, 4)).unwrap();
    terminal
        .draw(|frame| p.render(frame.area(), frame.buffer_mut()))
        .unwrap();
    let row = crate::testutil::row_text(&terminal.backend().buffer().clone(), 3);
    assert!(row.contains("Find: cat"), "{row:?}");
    assert!(row.contains("Replace: dog"), "{row:?}");
}

#[test]
fn the_caret_sits_in_the_field_being_typed_into() {
    let screen = Rect::new(0, 0, 60, 4);
    let mut p = find_prompt(true);
    type_str(&mut p, "cat");
    // "Find: " is 6 cells, plus "cat".
    assert_eq!(p.cursor(screen), Some(Position::new(9, 3)));
    p.handle_key(press(KeyCode::Tab));
    type_str(&mut p, "dog");
    // ...plus "  Replace: " (11) and "dog".
    assert_eq!(p.cursor(screen), Some(Position::new(23, 3)));
}

#[test]
fn a_zero_height_screen_draws_nothing_and_shows_no_cursor() {
    let p = find_prompt(false);
    let screen = Rect::new(0, 0, 0, 0);
    assert_eq!(p.cursor(screen), None);
    let mut buf = Buffer::empty(screen);
    p.render(screen, &mut buf);
    assert_eq!(buf, Buffer::empty(screen));
}

#[test]
fn the_caret_clamps_to_the_row_when_the_pattern_overruns_the_width() {
    let mut p = find_prompt(false);
    type_str(&mut p, "a-very-long-pattern-that-overflows");
    assert_eq!(p.cursor(Rect::new(0, 0, 10, 3)), Some(Position::new(9, 2)));
}

#[test]
fn restyling_adopts_the_new_palette() {
    let mut p = find_prompt(false);
    let theme = Theme {
        palette: Style::new().bg(Color::Rgb(9, 8, 7)),
        ..Theme::default()
    };
    p.restyle(&theme);
    assert_eq!(p.style, theme.palette);
}

// --- QueryReplace walk ----------------------------------------------------

fn walk() -> QueryReplace {
    QueryReplace::new(&Theme::default(), "cat".into(), "dog".into())
}

#[test]
fn yes_replaces_and_advances_without_ending_the_walk() {
    // The point of a walk is the next question.
    let mut w = walk();
    assert_eq!(w.handle_key(key('y')), EventResult::Consumed);
    assert!(!w.is_finished());
    assert_eq!(
        w.take_commands(),
        vec![
            Command::Editor(Action::ReplaceMatch {
                pattern: "cat".into(),
                replacement: "dog".into(),
            }),
            Command::Editor(Action::SelectNextMatch {
                pattern: "cat".into(),
                backward: false,
            }),
        ]
    );
}

#[test]
fn no_advances_without_replacing() {
    let mut w = walk();
    w.handle_key(key('n'));
    assert!(!w.is_finished());
    assert_eq!(
        w.take_commands(),
        vec![Command::Editor(Action::SelectNextMatch {
            pattern: "cat".into(),
            backward: false,
        })]
    );
}

#[test]
fn all_finishes_the_rest_in_one_edit_and_ends_the_walk() {
    let mut w = walk();
    w.handle_key(key('a'));
    assert!(w.is_finished());
    assert_eq!(
        w.take_commands(),
        vec![Command::Editor(Action::ReplaceAllMatches {
            pattern: "cat".into(),
            replacement: "dog".into(),
        })]
    );
}

#[test]
fn every_other_key_stops_the_walk_having_replaced_nothing() {
    // The destructive answers are exactly `y` and `a`, so a mistyped key can only
    // ever end the walk early.
    for stop in [
        key('q'),
        press(KeyCode::Esc),
        key('z'),
        press(KeyCode::Enter),
    ] {
        let mut w = walk();
        assert_eq!(w.handle_key(stop), EventResult::Consumed);
        assert!(w.is_finished(), "one keypress always answers: {stop:?}");
        assert_eq!(
            w.take_commands(),
            vec![Command::ClearSearch],
            "{stop:?} committed an edit"
        );
    }
}

#[test]
fn uppercase_answers_count_too() {
    for (answer, finishes) in [('Y', false), ('N', false), ('A', true)] {
        let mut w = walk();
        w.handle_key(key(answer));
        assert_eq!(w.is_finished(), finishes, "{answer}");
        assert!(
            w.take_commands()
                .iter()
                .any(|c| matches!(c, Command::Editor(_))),
            "{answer} did nothing"
        );
    }
}

#[test]
fn a_click_never_answers_yes() {
    // A misclick must not rewrite text, wherever it lands.
    let screen = Rect::new(0, 0, 40, 10);
    for (x, y) in [(5, 9), (5, 0)] {
        let mut w = walk();
        assert_eq!(w.handle_mouse(click(x, y), screen), EventResult::Consumed);
        assert!(w.is_finished());
        assert_eq!(w.take_commands(), vec![Command::ClearSearch]);
    }
}

#[test]
fn a_ctrl_chord_over_the_walk_is_deferred() {
    let mut w = walk();
    let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(w.handle_key(ctrl_s), EventResult::Ignored);
    assert!(!w.is_finished());
    assert!(w.take_commands().is_empty());
}

#[test]
fn the_question_names_both_halves_of_the_replacement() {
    // The user has to be able to see what they are agreeing to.
    let w = walk();
    let mut terminal = Terminal::new(TestBackend::new(70, 3)).unwrap();
    terminal
        .draw(|frame| w.render(frame.area(), frame.buffer_mut()))
        .unwrap();
    let row = crate::testutil::row_text(&terminal.backend().buffer().clone(), 2);
    assert!(row.contains("`cat`"), "{row:?}");
    assert!(row.contains("`dog`"), "{row:?}");
    assert!(row.contains("(y)es"), "{row:?}");
    // A question takes no typing, so it shows no caret.
    assert_eq!(w.cursor(Rect::new(0, 0, 70, 3)), None);
}

#[test]
fn a_zero_height_screen_draws_no_walk() {
    let w = walk();
    let screen = Rect::new(0, 0, 0, 0);
    let mut buf = Buffer::empty(screen);
    w.render(screen, &mut buf);
    assert_eq!(buf, Buffer::empty(screen));
}

#[test]
fn restyling_the_walk_adopts_the_new_palette() {
    let mut w = walk();
    let theme = Theme {
        palette: Style::new().bg(Color::Rgb(1, 2, 3)),
        ..Theme::default()
    };
    w.restyle(&theme);
    assert_eq!(w.style, theme.palette);
}
