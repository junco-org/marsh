use super::types::SelectionMode;
use super::*;
use rmux_core::input::InputParser;
use rmux_proto::TerminalSize;

/// At 10x8, `ABCDEFGHIJKLMNO` wraps onto a second physical row.
const WRAPPED_LINES: &str = "ABCDEFGHIJKLMNO\r\none  \r\ntwo\r\n";

/// The eight direct line-transfer commands as `(command, pipe, cancel, end_of_line)`.
const LINE_TRANSFER_COMMANDS: [(&str, bool, bool, bool); 8] = [
    ("copy-line", false, false, false),
    ("copy-line-and-cancel", false, true, false),
    ("copy-pipe-line", true, false, false),
    ("copy-pipe-line-and-cancel", true, true, false),
    ("copy-end-of-line", false, false, true),
    ("copy-end-of-line-and-cancel", false, true, true),
    ("copy-pipe-end-of-line", true, false, true),
    ("copy-pipe-end-of-line-and-cancel", true, true, true),
];

fn build_screen(cols: u16, rows: u16, content: &str) -> Screen {
    let mut screen = Screen::new(TerminalSize { cols, rows }, 200);
    let mut parser = InputParser::new();
    parser.parse(content.as_bytes(), &mut screen);
    screen
}

fn test_context() -> CopyModeCommandContext {
    CopyModeCommandContext {
        mode_keys: ModeKeys::Emacs,
        line_number_mode: CopyModeLineNumberMode::Off,
        wrap_search: true,
        word_separators: " -_@".to_owned(),
        default_shell: "/bin/sh".to_owned(),
        working_directory: None,
        refresh_screen: None,
        mouse: None,
    }
}

fn vi_context() -> CopyModeCommandContext {
    CopyModeCommandContext {
        mode_keys: ModeKeys::Vi,
        ..test_context()
    }
}

fn mouse_context(x: u32, y: u16) -> CopyModeCommandContext {
    CopyModeCommandContext {
        mouse: Some(CopyModeMouseContext {
            content_x: x,
            content_y: y,
            selection_anchor: None,
            scroll_y: y,
            slider_mpos: -1,
            move_cursor_before_command: true,
        }),
        ..test_context()
    }
}

fn test_state(cols: u16, rows: u16, content: &str) -> CopyModeState {
    CopyModeState::for_test(build_screen(cols, rows, content))
}

fn vi_state(cols: u16, rows: u16, content: &str) -> CopyModeState {
    let screen = build_screen(cols, rows, content);
    CopyModeState::new(screen, None, false, &vi_context(), false, true)
}

/// `foo.bar baz-qux end` scrolled to the top, with `.` as an extra word separator.
fn punctuated_state() -> (CopyModeState, CopyModeCommandContext) {
    let ctx = CopyModeCommandContext {
        word_separators: " .-_@".to_owned(),
        ..test_context()
    };
    let mut state = test_state(30, 5, "foo.bar baz-qux end\r\n");
    state.execute_command("history-top", &[], &ctx).unwrap();
    (state, ctx)
}

/// Runs argument-less `commands` in order, ignoring their results.
fn run(state: &mut CopyModeState, ctx: &CopyModeCommandContext, commands: &[&str]) {
    for command in commands {
        let _ = state.execute_command(command, &[], ctx);
    }
}

/// Runs argument-less `commands` in order; each one must succeed.
fn run_ok(state: &mut CopyModeState, ctx: &CopyModeCommandContext, commands: &[&str]) {
    for command in commands {
        state.execute_command(command, &[], ctx).unwrap();
    }
}

fn search_args(pattern: &str) -> [String; 2] {
    ["--".to_owned(), pattern.to_owned()]
}

fn pipe_args(pipe: bool) -> Vec<String> {
    if pipe {
        vec!["cat".to_owned()]
    } else {
        Vec::new()
    }
}

/// Runs the argument-less `command` and returns the bytes it transfers.
fn transferred(state: &mut CopyModeState, ctx: &CopyModeCommandContext, command: &str) -> Vec<u8> {
    state
        .execute_command(command, &[], ctx)
        .unwrap()
        .transfer
        .unwrap()
        .data
}

/// Runs `commands` (ignoring results); `copy` must then transfer exactly `expected`.
fn assert_transfer(
    mut state: CopyModeState,
    ctx: &CopyModeCommandContext,
    commands: &[&str],
    copy: &str,
    expected: &[u8],
) {
    run(&mut state, ctx, commands);
    assert_eq!(transferred(&mut state, ctx, copy), expected);
}

/// Runs `history-top`, then searches forward for `needle`, ignoring both results.
fn search_needle_from_top(state: &mut CopyModeState, ctx: &CopyModeCommandContext) {
    let _ = state.execute_command("history-top", &[], ctx);
    let _ = state.execute_command("search-forward", &search_args("needle"), ctx);
}

/// `command` must not cancel after `history-top` but must cancel after `history-bottom`.
fn assert_cancels_only_at_bottom(content: &str, command: &str, messages: [&str; 2]) {
    let mut state = test_state(20, 3, content);
    let ctx = test_context();

    let _ = state.execute_command("history-top", &[], &ctx);
    let outcome = state.execute_command(command, &[], &ctx).unwrap();
    assert!(!outcome.cancel, "{}", messages[0]);

    let _ = state.execute_command("history-bottom", &[], &ctx);
    let outcome = state.execute_command(command, &[], &ctx).unwrap();
    assert!(outcome.cancel, "{}", messages[1]);
}

fn exit_on_scroll_state() -> CopyModeState {
    let screen = build_screen(20, 3, "line1\r\nline2\r\nline3\r\nline4\r\nline5");
    CopyModeState::new(
        screen,
        None,
        false,
        &test_context(),
        true, // exit_on_scroll
        true,
    )
}

/// Repeats `command` from `start` in [`punctuated_state`], checking every cursor stop.
fn assert_punctuated_stops(start: Option<CopyPosition>, command: &str, stops: &[CopyPosition]) {
    let (mut state, ctx) = punctuated_state();
    if let Some(start) = start {
        state.cursor = start;
    }

    for &expected in stops {
        state.execute_command(command, &[], &ctx).unwrap();
        assert_eq!(state.cursor, expected);
    }
}

fn assert_mouse_selection(select: &str, expected: &[u8]) {
    let mut state = test_state(20, 3, "alpha beta\r\ngamma delta\r\n");

    let _ = state.execute_command("history-top", &[], &test_context());
    let _ = state.execute_command(select, &[], &mouse_context(6, 1));

    assert_eq!(
        transferred(&mut state, &test_context(), "copy-selection"),
        expected
    );
}

/// Selects the whole wrapped first line character-wise; `copy` must not add newlines.
fn assert_wrapped_character_selection(copy: &str) {
    let mut state = test_state(10, 8, "0123456789ABCDEFGHIJKLMNO\r\n");
    let ctx = test_context();

    run_ok(
        &mut state,
        &ctx,
        &[
            "history-top",
            "start-of-line",
            "begin-selection",
            "end-of-line",
        ],
    );

    assert_eq!(
        transferred(&mut state, &ctx, copy),
        b"0123456789ABCDEFGHIJKLMNO"
    );
}

/// Runs every [`LINE_TRANSFER_COMMANDS`] entry with `-N3` from the top of [`WRAPPED_LINES`].
fn assert_counted_line_transfers(context: &CopyModeCommandContext) {
    let screen = build_screen(10, 8, WRAPPED_LINES);

    for (command, pipe, cancel, end_of_line) in LINE_TRANSFER_COMMANDS {
        let mut state = CopyModeState::for_test(screen.clone());
        state.execute_command("history-top", &[], context).unwrap();
        if end_of_line {
            run_ok(&mut state, context, &["cursor-right", "cursor-right"]);
        }

        let outcome = state
            .execute_command_with_prefix(command, &pipe_args(pipe), context, 3)
            .unwrap();
        let transfer = outcome.transfer.expect("counted line transfer is produced");
        let expected = if end_of_line {
            b"CDEFGHIJKLMNO\none".as_slice()
        } else {
            b"ABCDEFGHIJKLMNO\none".as_slice()
        };

        assert_eq!(transfer.data, expected, "{command}");
        assert_eq!(transfer.pipe_command.is_some(), pipe, "{command}");
        assert_eq!(outcome.cancel, cancel, "{command}");
    }
}

#[test]
fn summary_top_line_time_is_zero_for_visible_lines_at_bottom() {
    let state = test_state(20, 5, "line1\r\nline2\r\n");

    assert_eq!(state.summary().top_line_time, 0);
}

#[test]
fn summary_top_line_time_is_preserved_for_history_lines() {
    let mut state = test_state(
        20,
        3,
        "line1\r\nline2\r\nline3\r\nline4\r\nline5\r\nline6\r\n",
    );
    let _ = state.execute_command("history-top", &[], &test_context());

    assert!(
        state.summary().top_line_time > 0,
        "history lines should keep their timestamp for copy-mode-position-format"
    );
}

#[test]
fn cursor_down_and_cancel_only_cancels_at_bottom() {
    assert_cancels_only_at_bottom(
        "line1\r\nline2\r\nline3",
        "cursor-down-and-cancel",
        [
            "should not cancel when cursor moved down",
            "should cancel when at bottom and cursor did not move",
        ],
    );
}

#[test]
fn scroll_down_and_cancel_only_cancels_at_bottom() {
    assert_cancels_only_at_bottom(
        "line1\r\nline2\r\nline3\r\nline4\r\nline5",
        "scroll-down-and-cancel",
        [
            "should not cancel when not at bottom",
            "should cancel when at bottom",
        ],
    );
}

#[test]
fn exit_on_scroll_cancels_scroll_down_at_bottom() {
    let mut state = exit_on_scroll_state();

    // At the bottom already.
    let outcome = state
        .execute_command("scroll-down", &[], &test_context())
        .unwrap();
    assert!(
        outcome.cancel,
        "scroll-down should cancel with exit_on_scroll at bottom"
    );
}

#[test]
fn exit_on_scroll_does_not_cancel_when_not_at_bottom() {
    let mut state = exit_on_scroll_state();
    let ctx = test_context();

    let _ = state.execute_command("history-top", &[], &ctx);

    let outcome = state.execute_command("scroll-down", &[], &ctx).unwrap();
    assert!(
        !outcome.cancel,
        "scroll-down should not cancel when not at bottom"
    );
}

#[test]
fn search_again_advances_to_next_match() {
    let mut state = test_state(20, 3, "foo bar foo baz foo");
    let ctx = test_context();

    let _ = state.execute_command("search-forward", &search_args("foo"), &ctx);
    let first = state.cursor;

    let _ = state.execute_command("search-again", &[], &ctx);
    let second = state.cursor;
    assert!(
        second.x > first.x || second.y > first.y,
        "search-again should advance: first={:?}, second={:?}",
        first,
        second,
    );
}

#[test]
fn search_updates_an_active_selection_endpoint() {
    let mut state = test_state(20, 3, "alpha beta gamma");
    let context = test_context();

    run_ok(&mut state, &context, &["history-top", "begin-selection"]);
    state
        .execute_command("search-forward", &search_args("beta"), &context)
        .unwrap();

    assert_eq!(state.summary().selection_end, Some(state.cursor));
}

#[test]
fn case_insensitive_plain_search_maps_unicode_expansion_to_original_cell() {
    let mut state = test_state(10, 1, "İx");

    state
        .execute_command("search-forward-text", &["x".to_owned()], &test_context())
        .unwrap();

    assert_eq!(state.search_results.len(), 1);
    assert_eq!(state.search_results[0].text, "x");
    assert_eq!(
        state.cursor.x, 1,
        "the match must stay on the original x cell"
    );
}

#[test]
fn oversized_regex_search_marks_partial_without_matches() {
    let mut state = test_state(80, 3, &"a".repeat(1000));

    state
        .execute_command("search-forward", &search_args("a{50000}"), &test_context())
        .unwrap();

    let summary = state.summary();
    assert!(summary.search_count_partial);
    assert_eq!(summary.search_count, 0);
}

#[test]
fn search_again_respects_wrap_search_off() {
    let screen = build_screen(30, 3, "foo bar foo baz foo");
    let context = CopyModeCommandContext {
        wrap_search: false,
        ..test_context()
    };
    let mut state = CopyModeState::new(screen, None, false, &context, false, true);
    state.cursor = CopyPosition { x: 0, y: 0 };

    let _ = state.execute_command("search-forward", &search_args("foo"), &context);
    run(&mut state, &context, &["search-again", "search-again"]);
    let last = state.cursor;

    let _ = state.execute_command("search-again", &[], &context);

    assert_eq!(state.cursor, last, "search-again should not wrap");
}

#[test]
fn search_reverse_goes_backward_without_changing_direction() {
    let mut state = test_state(30, 3, "foo bar foo baz foo more text");
    let ctx = test_context();

    // Initial forward search.
    let _ = state.execute_command("history-top", &[], &ctx);
    let _ = state.execute_command("search-forward", &search_args("foo"), &ctx);
    let _ = state.execute_command("search-again", &[], &ctx);
    let before_reverse = state.cursor;

    // search-reverse should go backward.
    let _ = state.execute_command("search-reverse", &[], &ctx);
    let after_reverse = state.cursor;
    assert!(
        after_reverse.x < before_reverse.x || after_reverse.y < before_reverse.y,
        "search-reverse should go backward: before={:?}, after={:?}",
        before_reverse,
        after_reverse,
    );

    // search-again should still go forward (direction unchanged).
    let _ = state.execute_command("search-again", &[], &ctx);
    let after_again = state.cursor;
    assert!(
        after_again.x > after_reverse.x || after_again.y > after_reverse.y,
        "search-again should still go forward: reverse={:?}, again={:?}",
        after_reverse,
        after_again,
    );
}

#[test]
fn vi_search_positions_at_match_start() {
    let mut state = vi_state(30, 3, "hello needle world");

    search_needle_from_top(&mut state, &vi_context());
    assert_eq!(
        state.cursor.x, 6,
        "vi search should position at match start"
    );
}

#[test]
fn jump_to_forward_moves_before_the_matched_character_and_repeats() {
    let mut state = test_state(20, 3, "aXbXcXdXe");
    let ctx = test_context();

    state.execute_command("history-top", &[], &ctx).unwrap();
    state
        .execute_command("jump-to-forward", &["X".to_owned()], &ctx)
        .unwrap();
    assert_eq!(state.cursor, CopyPosition { x: 2, y: 0 });

    state.execute_command("jump-again", &[], &ctx).unwrap();
    assert_eq!(state.cursor, CopyPosition { x: 4, y: 0 });
}

#[test]
fn jump_to_backward_skips_adjacent_match_and_repeats() {
    let mut state = test_state(20, 3, "aXbXcXdXe");
    let ctx = test_context();

    run_ok(&mut state, &ctx, &["history-top", "end-of-line"]);

    state
        .execute_command("jump-to-backward", &["X".to_owned()], &ctx)
        .unwrap();
    assert_eq!(state.cursor, CopyPosition { x: 8, y: 0 });

    for x in [6, 4, 2] {
        state.execute_command("jump-again", &[], &ctx).unwrap();
        assert_eq!(state.cursor, CopyPosition { x, y: 0 });
    }
}

#[test]
fn jump_to_backward_skips_adjacent_wide_match() {
    let mut state = test_state(20, 3, "界a界b");
    let ctx = test_context();

    state.execute_command("history-top", &[], &ctx).unwrap();
    state.cursor = CopyPosition { x: 5, y: 0 };
    state
        .execute_command("jump-to-backward", &["界".to_owned()], &ctx)
        .unwrap();

    assert_eq!(state.cursor, CopyPosition { x: 2, y: 0 });
}

#[test]
fn jump_to_forward_skips_adjacent_match_like_tmux() {
    let mut state = test_state(20, 3, "abXd");
    let ctx = test_context();

    state.execute_command("history-top", &[], &ctx).unwrap();
    state
        .execute_command("jump-to-forward", &["X".to_owned()], &ctx)
        .unwrap();

    assert_eq!(state.cursor, CopyPosition { x: 1, y: 0 });
}

#[test]
fn next_word_stops_on_separator_tokens_like_tmux() {
    assert_punctuated_stops(
        None,
        "next-word",
        &[
            CopyPosition { x: 3, y: 0 },
            CopyPosition { x: 4, y: 0 },
            CopyPosition { x: 8, y: 0 },
            CopyPosition { x: 11, y: 0 },
            CopyPosition { x: 12, y: 0 },
            CopyPosition { x: 16, y: 0 },
            CopyPosition { x: 1, y: 4 },
        ],
    );
}

#[test]
fn next_word_end_stops_after_current_token_like_tmux() {
    assert_punctuated_stops(
        None,
        "next-word-end",
        &[
            CopyPosition { x: 3, y: 0 },
            CopyPosition { x: 4, y: 0 },
            CopyPosition { x: 7, y: 0 },
            CopyPosition { x: 11, y: 0 },
            CopyPosition { x: 12, y: 0 },
            CopyPosition { x: 15, y: 0 },
            CopyPosition { x: 19, y: 0 },
            CopyPosition { x: 0, y: 4 },
        ],
    );
}

#[test]
fn previous_word_stops_on_separator_tokens_like_tmux() {
    assert_punctuated_stops(
        Some(CopyPosition { x: 16, y: 0 }),
        "previous-word",
        &[
            CopyPosition { x: 12, y: 0 },
            CopyPosition { x: 11, y: 0 },
            CopyPosition { x: 8, y: 0 },
            CopyPosition { x: 4, y: 0 },
            CopyPosition { x: 3, y: 0 },
            CopyPosition { x: 0, y: 0 },
        ],
    );
}

#[test]
fn next_space_moves_to_the_terminal_boundary_after_the_last_word_like_tmux() {
    assert_punctuated_stops(
        Some(CopyPosition { x: 16, y: 0 }),
        "next-space",
        &[CopyPosition { x: 1, y: 4 }],
    );
}

#[test]
fn copy_cursor_word_uses_the_next_word_from_separators_like_tmux() {
    let (mut state, _) = punctuated_state();

    state.cursor = CopyPosition { x: 3, y: 0 };
    assert_eq!(state.summary().copy_cursor_word, "bar");

    state.cursor = CopyPosition { x: 11, y: 0 };
    assert_eq!(state.summary().copy_cursor_word, "qux");

    state.cursor = CopyPosition { x: 1, y: 4 };
    assert_eq!(state.summary().copy_cursor_word, "");
}

#[test]
fn matching_brackets_match_naive_ascii_oracle() {
    let text = "a(b[c{d}e]f) z";
    let mut state = test_state(30, 3, text);

    state
        .execute_command("history-top", &[], &test_context())
        .unwrap();
    for cursor_x in [1, 3, 5, 7, 9, 11] {
        state.cursor = CopyPosition { x: cursor_x, y: 0 };
        for forward in [true, false] {
            let expected =
                naive_matching_bracket(text, cursor_x, forward).map(|x| CopyPosition { x, y: 0 });
            assert_eq!(
                state.find_matching_bracket(forward),
                expected,
                "cursor_x={cursor_x}, forward={forward}"
            );
        }
    }
}

#[test]
fn matching_brackets_scan_across_lines_without_flattening_buffer() {
    let mut state = test_state(10, 6, "(\r\n[\r\n]\r\n)");
    state
        .execute_command("history-top", &[], &test_context())
        .unwrap();

    state.cursor = CopyPosition { x: 0, y: 0 };
    assert_eq!(
        state.find_matching_bracket(true),
        Some(CopyPosition { x: 0, y: 3 })
    );

    state.cursor = CopyPosition { x: 0, y: 3 };
    assert_eq!(
        state.find_matching_bracket(true),
        Some(CopyPosition { x: 0, y: 0 })
    );
}

fn naive_matching_bracket(text: &str, cursor_x: u32, forward: bool) -> Option<u32> {
    let chars = text.chars().collect::<Vec<_>>();
    let current = *chars.get(cursor_x as usize)?;
    let (open, close, scan_forward) = match current {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        '}' => ('{', '}', false),
        _ => return None,
    };
    let scan_forward = if forward { scan_forward } else { !scan_forward };
    let mut depth = 1usize;
    if scan_forward {
        for (index, ch) in chars
            .iter()
            .copied()
            .enumerate()
            .skip(cursor_x as usize + 1)
        {
            if ch == open {
                depth += 1;
            } else if ch == close {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index as u32);
                }
            }
        }
    } else {
        for (index, ch) in chars
            .iter()
            .copied()
            .enumerate()
            .take(cursor_x as usize)
            .rev()
        {
            if ch == close {
                depth += 1;
            } else if ch == open {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index as u32);
                }
            }
        }
    }
    None
}

#[test]
fn emacs_search_positions_past_match_end() {
    let mut state = test_state(30, 3, "hello needle world");

    search_needle_from_top(&mut state, &test_context());
    // "needle" starts at col 6, ends at col 11.
    assert_eq!(
        state.cursor.x, 11,
        "emacs search should position at match end"
    );
}

#[test]
fn view_mode_blocks_non_readonly_commands() {
    let screen = build_screen(20, 3, "hello world");
    let mut state = CopyModeState::new(
        screen,
        None,
        true, // view_mode
        &test_context(),
        false,
        true,
    );
    let ctx = test_context();

    // Readonly commands should work.
    let outcome = state.execute_command("cursor-down", &[], &ctx).unwrap();
    assert!(!outcome.cancel);

    // Non-readonly commands should be silently ignored.
    let outcome = state.execute_command("begin-selection", &[], &ctx).unwrap();
    assert!(!outcome.cancel);
    assert!(
        state.selection.is_none(),
        "view-mode should block begin-selection"
    );
}

#[test]
fn copy_selection_with_no_selection_yields_empty_data() {
    let mut state = test_state(20, 3, "hello");

    let outcome = state
        .execute_command("copy-selection-and-cancel", &[], &test_context())
        .unwrap();
    assert!(outcome.cancel);
    let transfer = outcome.transfer.unwrap();
    assert!(
        transfer.data.is_empty(),
        "should produce empty data when no selection"
    );
}

#[test]
fn character_selection_excludes_the_cursor_cell_like_tmux() {
    assert_transfer(
        test_state(20, 3, "alpha\r\nbeta\r\ngamma\r\n"),
        &test_context(),
        &[
            "history-top",
            "cursor-down",
            "begin-selection",
            "cursor-right",
        ],
        "copy-selection-and-cancel",
        b"b",
    );
}

#[test]
fn cursor_right_wraps_after_logical_line_end() {
    let mut state = test_state(30, 5, "foo.bar baz-qux end\r\n\r\n");
    let ctx = test_context();

    let _ = state.execute_command("history-top", &[], &ctx);
    for _ in 0..20 {
        let _ = state.execute_command("cursor-right", &[], &ctx);
    }

    let summary = state.summary();
    assert_eq!(summary.cursor_x, 0);
    assert_eq!(summary.cursor_y, 1);
    assert_eq!(summary.copy_cursor_line, "");
}

#[test]
fn multiline_character_selection_excludes_first_cell_of_end_line_like_tmux() {
    assert_transfer(
        test_state(20, 3, "alpha\r\nbeta\r\ngamma\r\n"),
        &test_context(),
        &[
            "history-top",
            "cursor-down",
            "begin-selection",
            "cursor-down",
        ],
        "copy-selection-and-cancel",
        b"beta\n",
    );
}

#[test]
fn middle_line_uses_upper_middle_on_even_height_like_tmux() {
    let mut state = test_state(
        20,
        6,
        "line1\r\nline2\r\nline3\r\nline4\r\nline5\r\nline6\r\n",
    );

    run(&mut state, &test_context(), &["history-top", "middle-line"]);

    assert_eq!(state.summary().cursor_y, 2);
    assert_eq!(state.summary().copy_cursor_line, "line3");
}

#[test]
fn recentre_top_bottom_cycles_for_same_cursor_line() {
    let mut state = test_state(
        20,
        5,
        "line1\r\nline2\r\nline3\r\nline4\r\nline5\r\nline6\r\nline7\r\nline8\r\nline9\r\nline10\r\n",
    );
    let ctx = test_context();
    state.cursor.y = 6;
    state.cursor.x = 0;
    state.top_line = 0;

    for top_line in [4, 6, 2] {
        let _ = state.execute_command("recentre-top-bottom", &[], &ctx);
        assert_eq!(state.top_line, top_line);
    }

    state.cursor.y = 7;
    let _ = state.execute_command("recentre-top-bottom", &[], &ctx);
    assert_eq!(state.top_line, 5);
}

#[test]
fn goto_line_scrolls_from_bottom_like_tmux() {
    let mut state = test_state(
        30,
        4,
        "L1\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6\r\nL7\r\nL8\r\nL9\r\nL10\r\nPROMPT",
    );
    let ctx = test_context();

    state.set_show_position(false);
    for (line, scroll_position, cursor_y, copy_cursor_line) in [
        ("1", 1, Some(3), "L10"),
        ("999", 7, Some(3), "L4"),
        ("bad", 7, None, "L4"),
        ("-5", 7, None, "L4"),
    ] {
        let _ = state.execute_command("goto-line", &[line.to_owned()], &ctx);
        let summary = state.summary();
        assert_eq!(summary.scroll_position, scroll_position, "goto-line {line}");
        if let Some(cursor_y) = cursor_y {
            assert_eq!(summary.cursor_y, cursor_y, "goto-line {line}");
        }
        assert_eq!(
            summary.copy_cursor_line, copy_cursor_line,
            "goto-line {line}"
        );
    }
}

#[test]
fn goto_line_uses_top_relative_positions_for_absolute_line_numbers() {
    let mut state = test_state(
        30,
        4,
        "L1\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6\r\nL7\r\nL8\r\nL9\r\nL10\r\nPROMPT",
    );
    let ctx = CopyModeCommandContext {
        line_number_mode: CopyModeLineNumberMode::Absolute,
        ..test_context()
    };

    // Measured against tmux 3.7b: with hsize=7, absolute goto-line 1
    // selects the oldest history viewport, while hsize+1 selects the bottom.
    for (line, scroll_position) in [("1", 7), ("8", 0), ("0", 7), ("-1", 7)] {
        let _ = state.execute_command("goto-line", &[line.to_owned()], &ctx);
        assert_eq!(
            state.summary().scroll_position,
            scroll_position,
            "goto-line {line}"
        );
    }

    state.set_line_numbers_enabled(false);
    let _ = state.execute_command("goto-line", &["1".to_owned()], &ctx);
    assert_eq!(
        state.summary().scroll_position,
        1,
        "a mouse-origin copy-mode entry keeps tmux's bottom-relative semantics"
    );
}

#[test]
fn line_selection_omits_trailing_newline() {
    let mut state = test_state(20, 3, "alpha\r\nbeta\r\ngamma\r\n");
    let ctx = test_context();

    run(&mut state, &ctx, &["history-top", "select-line"]);
    assert_eq!(state.summary().cursor_x, 5);
    assert_eq!(
        state.summary().selection_end.unwrap(),
        CopyPosition { x: 5, y: 0 }
    );

    assert_eq!(transferred(&mut state, &ctx, "copy-selection"), b"alpha");
}

#[test]
fn line_selection_uses_mouse_position_when_available() {
    assert_mouse_selection("select-line", b"gamma delta");
}

#[test]
fn word_selection_uses_mouse_position_when_available() {
    assert_mouse_selection("select-word", b"delta");
}

#[test]
fn generic_mouse_reposition_preserves_the_active_selection_endpoint() {
    let mut state = test_state(20, 3, "alpha beta\r\ngamma delta\r\n");

    run_ok(&mut state, &test_context(), &["history-top", "select-word"]);
    let outcome = state
        .execute_command("copy-selection", &[], &mouse_context(1, 1))
        .unwrap();

    // tmux 3.7b moves the cursor for mouse-origin send -X without changing
    // the selection endpoints recorded by select-word.
    assert_eq!(state.cursor, CopyPosition { x: 1, y: 1 });
    assert_eq!(outcome.transfer.unwrap().data, b"alpha");
}

#[test]
fn vi_line_selection_includes_trailing_newline() {
    assert_transfer(
        vi_state(20, 3, "alpha\r\nbeta\r\ngamma\r\n"),
        &vi_context(),
        &["history-top", "select-line"],
        "copy-selection",
        b"alpha\n",
    );
}

#[test]
fn line_selection_keeps_internal_newlines_without_trailing_newline() {
    assert_transfer(
        test_state(20, 3, "alpha\r\nbeta\r\ngamma\r\n"),
        &test_context(),
        &["history-top", "select-line", "cursor-down"],
        "copy-selection",
        b"alpha\nbeta",
    );
}

#[test]
fn line_selection_joins_wrapped_physical_rows_without_trailing_newline() {
    assert_transfer(
        test_state(20, 4, "ABCDEFGHIJKLMNOPQRSTUVWXYZ\r\n"),
        &test_context(),
        &["history-top", "select-line"],
        "copy-selection",
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZ",
    );
}

#[test]
fn character_selection_joins_wrapped_physical_rows_without_newlines() {
    assert_wrapped_character_selection("copy-selection");
}

#[test]
fn copy_pipe_uses_wrapped_character_selection_without_newlines() {
    assert_wrapped_character_selection("copy-pipe");
}

#[test]
fn copy_line_omits_trailing_newline() {
    assert_transfer(
        test_state(20, 3, "alpha\r\nbeta\r\n"),
        &test_context(),
        &["history-top"],
        "copy-line",
        b"alpha",
    );
}

#[test]
fn counted_line_transfers_preserve_wrapped_rows_and_line_boundaries() {
    // Oracle tmux 3.7b: at width 10, -N3 counts the two physical rows of
    // ABCDEFGHIJKLMNO plus the following row, while -N4 also includes "two".
    let screen = build_screen(10, 8, WRAPPED_LINES);
    let context = test_context();

    for (command, count, cursor_x, expected) in [
        ("copy-line", 1, 0, b"ABCDEFGHIJKLMNO".as_slice()),
        ("copy-line", 3, 0, b"ABCDEFGHIJKLMNO\none".as_slice()),
        ("copy-line", 4, 0, b"ABCDEFGHIJKLMNO\none\ntwo".as_slice()),
        ("copy-end-of-line", 1, 2, b"CDEFGHIJKLMNO".as_slice()),
        ("copy-end-of-line", 3, 2, b"CDEFGHIJKLMNO\none".as_slice()),
        (
            "copy-end-of-line",
            4,
            2,
            b"CDEFGHIJKLMNO\none\ntwo".as_slice(),
        ),
    ] {
        let mut state = CopyModeState::for_test(screen.clone());
        state.execute_command("history-top", &[], &context).unwrap();
        for _ in 0..cursor_x {
            state
                .execute_command("cursor-right", &[], &context)
                .unwrap();
        }

        let outcome = state
            .execute_command_with_prefix(command, &[], &context, count)
            .unwrap();

        assert_eq!(
            outcome.transfer.unwrap().data,
            expected,
            "{command} -N{count}"
        );
    }
}

#[test]
fn counted_line_transfers_distinguish_wrapped_logical_and_physical_starts() {
    // Oracle tmux 3.7b from the second physical row of ABCDEFGHIJKLMNO:
    // copy-line restarts at the logical beginning, while copy-end-of-line
    // counts from the physical row containing the cursor.
    let screen = build_screen(10, 8, WRAPPED_LINES);
    let context = test_context();

    for (command, count, expected) in [
        ("copy-line", 1, b"ABCDEFGHIJKLMNO".as_slice()),
        ("copy-line", 2, b"ABCDEFGHIJKLMNO".as_slice()),
        ("copy-line", 3, b"ABCDEFGHIJKLMNO\none".as_slice()),
        ("copy-end-of-line", 1, b"KLMNO".as_slice()),
        ("copy-end-of-line", 2, b"KLMNO\none".as_slice()),
        ("copy-end-of-line", 3, b"KLMNO\none\ntwo".as_slice()),
    ] {
        let mut state = CopyModeState::for_test(screen.clone());
        state.execute_command("history-top", &[], &context).unwrap();
        state.cursor = CopyPosition { x: 0, y: 1 };

        let outcome = state
            .execute_command_with_prefix(command, &[], &context, count)
            .unwrap();

        assert_eq!(
            outcome.transfer.unwrap().data,
            expected,
            "{command} -N{count}"
        );
    }
}

#[test]
fn line_transfer_family_consumes_prefix_as_one_counted_command() {
    // Oracle tmux 3.7b: every command below consumes -N3 as one counted
    // transfer. Pipe variants launch once; and-cancel variants then exit mode.
    for (command, ..) in LINE_TRANSFER_COMMANDS {
        assert_eq!(
            CopyModeState::prefix_behavior(command),
            CopyModePrefixBehavior::Count
        );
    }
    assert_counted_line_transfers(&test_context());

    // Oracle tmux 3.7b: cursor-right -N3 reaches x=3, so ordinary motion
    // remains repeat-based when counted line transfers move to one execution.
    assert_eq!(
        CopyModeState::prefix_behavior("cursor-right"),
        CopyModePrefixBehavior::Repeat
    );
}

#[test]
fn counted_vi_line_transfers_do_not_use_the_selection_newline() {
    // Oracle tmux 3.7b: counted direct transfers have the same bytes in vi and
    // emacs modes; only an explicit vi line selection gains a trailing LF.
    assert_counted_line_transfers(&vi_context());
}

#[test]
fn vi_copy_line_omits_trailing_newline() {
    assert_transfer(
        vi_state(20, 3, "alpha\r\nbeta\r\n"),
        &vi_context(),
        &["history-top"],
        "copy-line",
        b"alpha",
    );
}

#[test]
fn copy_line_on_empty_line_yields_empty_data() {
    assert_transfer(
        test_state(20, 3, "\r\n"),
        &test_context(),
        &["history-top"],
        "copy-line",
        b"",
    );
}

#[test]
fn vi_copy_line_on_empty_line_yields_empty_data() {
    assert_transfer(
        vi_state(20, 3, "\r\n"),
        &vi_context(),
        &["history-top"],
        "copy-line",
        b"",
    );
}

#[test]
fn copy_end_of_line_omits_trailing_newline() {
    assert_transfer(
        test_state(20, 3, "alpha\r\nbeta\r\n"),
        &test_context(),
        &["history-top"],
        "copy-end-of-line",
        b"alpha",
    );
}

#[test]
fn vi_copy_end_of_line_omits_trailing_newline() {
    assert_transfer(
        vi_state(20, 3, "alpha\r\nbeta\r\n"),
        &vi_context(),
        &["history-top"],
        "copy-end-of-line",
        b"alpha",
    );
}

#[test]
fn vi_direct_line_transfer_family_keeps_selection_newlines_out_of_payloads() {
    // Oracle tmux 3.7b: vi line selections retain a trailing newline, but the
    // eight direct line-transfer commands do not add one of their own.
    let context = vi_context();
    for (command, pipe, cancel, _) in LINE_TRANSFER_COMMANDS {
        let mut state = vi_state(20, 3, "alpha\r\nbeta\r\n");
        state.execute_command("history-top", &[], &context).unwrap();

        let outcome = state
            .execute_command(command, &pipe_args(pipe), &context)
            .unwrap();
        let transfer = outcome.transfer.expect("direct line transfer");

        assert_eq!(transfer.data, b"alpha", "{command}");
        assert_eq!(transfer.pipe_command.is_some(), pipe, "{command}");
        assert_eq!(outcome.cancel, cancel, "{command}");
    }
}

#[test]
fn end_of_line_moves_to_wrapped_logical_line_end_like_tmux() {
    let mut state = test_state(20, 4, "ABCDEFGHIJKLMNOPQRSTUVWXYZ\r\n");

    run(&mut state, &test_context(), &["history-top", "end-of-line"]);

    assert_eq!(state.summary().cursor_x, 6);
    assert_eq!(state.summary().cursor_y, 1);
}

#[test]
fn copy_end_of_line_uses_wrapped_logical_line_without_trailing_newline() {
    assert_transfer(
        test_state(20, 4, "ABCDEFGHIJKLMNOPQRSTUVWXYZ\r\n"),
        &test_context(),
        &["history-top"],
        "copy-end-of-line",
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZ",
    );
}

#[test]
fn end_of_line_stops_after_content_like_tmux() {
    let mut state = test_state(20, 3, "alpha\r\nbeta\r\ngamma\r\n");

    run(&mut state, &test_context(), &["history-top", "end-of-line"]);

    assert_eq!(state.summary().cursor_x, 5);
}

#[test]
fn clear_policy_emacs_only_clears_in_emacs_mode() {
    let mut state = test_state(30, 3, "hello world needle");
    let ctx = test_context();

    search_needle_from_top(&mut state, &ctx);
    assert!(state.search_highlighted);

    // cursor-down has EmacsOnly clear policy; in emacs mode it should clear.
    let _ = state.execute_command("cursor-down", &[], &ctx);
    assert!(
        !state.search_highlighted,
        "emacs mode cursor-down should clear highlights"
    );
}

#[test]
fn clear_policy_emacs_only_does_not_clear_in_vi_mode() {
    let mut state = vi_state(30, 3, "hello world needle");
    let ctx = vi_context();

    search_needle_from_top(&mut state, &ctx);
    assert!(state.search_highlighted);

    // cursor-down has EmacsOnly clear policy; in vi mode it should NOT clear.
    let _ = state.execute_command("cursor-down", &[], &ctx);
    assert!(
        state.search_highlighted,
        "vi mode cursor-down should not clear highlights"
    );
}

#[test]
fn selection_mode_switches_existing_selection() {
    let mut state = test_state(30, 3, "hello world");
    let ctx = test_context();

    let _ = state.execute_command("begin-selection", &[], &ctx);
    assert_eq!(state.selection.as_ref().unwrap().mode, SelectionMode::Char);

    let _ = state.execute_command("selection-mode", &["word".to_owned()], &ctx);
    assert_eq!(state.selection.as_ref().unwrap().mode, SelectionMode::Word);
}

#[test]
fn mark_and_jump_to_mark() {
    let mut state = test_state(20, 5, "line1\r\nline2\r\nline3\r\nline4\r\nline5");
    let ctx = test_context();

    run(&mut state, &ctx, &["history-top", "set-mark"]);
    let mark_pos = state.cursor;

    run(&mut state, &ctx, &["cursor-down", "cursor-down"]);
    assert_ne!(state.cursor, mark_pos);

    let _ = state.execute_command("jump-to-mark", &[], &ctx);
    assert_eq!(state.cursor, mark_pos, "should jump back to mark position");
}

#[test]
fn unknown_command_returns_error() {
    let mut state = test_state(20, 3, "hello");

    let result = state.execute_command("not-a-real-command", &[], &test_context());
    assert!(result.is_err());
}

#[test]
fn rg15_next_word_at_end_lands_after_last_word_when_last_row_is_populated() {
    let mut state = test_state(30, 3, "alpha\r\nbeta\r\ngamma");
    let ctx = test_context();

    state.execute_command("history-top", &[], &ctx).unwrap();
    assert_eq!(state.cursor, CopyPosition { x: 0, y: 0 });

    for _ in 0..10 {
        state.execute_command("next-word", &[], &ctx).unwrap();
    }
    assert_eq!(state.cursor, CopyPosition { x: 5, y: 2 });
    assert_eq!(state.summary().copy_cursor_word, "");
}

#[test]
fn rg15_next_word_single_line_lands_after_only_word() {
    let mut state = test_state(30, 1, "alpharbeta");
    let ctx = test_context();

    state.execute_command("history-top", &[], &ctx).unwrap();
    state.cursor = CopyPosition { x: 0, y: 0 };

    state.execute_command("next-word", &[], &ctx).unwrap();
    let after_first = state.cursor;
    state.execute_command("next-word", &[], &ctx).unwrap();
    let after_second = state.cursor;

    assert_eq!(after_first, CopyPosition { x: 10, y: 0 });
    assert_eq!(after_second, CopyPosition { x: 10, y: 0 });
    assert_eq!(state.summary().copy_cursor_word, "");
}

#[test]
fn next_word_at_full_width_final_line_stays_at_logical_end() {
    let mut state = test_state(5, 1, "abcde");

    run_ok(
        &mut state,
        &test_context(),
        &["history-top", "next-word", "next-word"],
    );

    assert_eq!(state.cursor, CopyPosition { x: 5, y: 0 });
    assert_eq!(state.summary().copy_cursor_word, "");
}

#[test]
fn rg15_next_space_at_end_lands_after_last_word_when_last_row_is_populated() {
    let mut state = test_state(30, 3, "alpha\r\nbeta\r\ngamma");
    let ctx = test_context();

    state.execute_command("history-top", &[], &ctx).unwrap();
    for _ in 0..10 {
        state.execute_command("next-space", &[], &ctx).unwrap();
    }
    assert_eq!(state.cursor, CopyPosition { x: 5, y: 2 });
    assert_eq!(state.summary().copy_cursor_word, "");
}
