use super::{
    border_cells, parse_standalone_style, render, status_bar_runs, style_sgr_bytes, BorderStyle,
};
use crate::copy_mode::CopyModeSummary;
use crate::handler::RequestHandler;
use rmux_core::{
    input::{mode, InputParser},
    OptionStore, Screen, Session, Style, Utf8Config,
};
use rmux_proto::{
    OptionName, ResizePaneAdjustment, ScopeSelector, SetOptionMode, SplitDirection, TerminalSize,
    WindowTarget,
};

use crate::test_fixtures::option_store;
use crate::test_names::session_name;

fn alpha_session(size: TerminalSize) -> Session {
    Session::new(session_name("alpha"), size)
}

fn split_alpha_session(size: TerminalSize) -> Session {
    let mut session = alpha_session(size);
    session.split_active_pane().expect("split succeeds");
    session
}

fn session_with_three_panes() -> Session {
    let mut session = split_alpha_session(TerminalSize::new(80, 24));
    session.split_pane(1).expect("second split succeeds");
    session
}

fn border_style(value: Option<&str>) -> Style {
    parse_standalone_style(value)
}

fn window_border_cells(
    session: &Session,
    inactive: BorderStyle,
    active: BorderStyle,
) -> Vec<super::BorderCell> {
    border_cells(
        session.window(),
        session.active_pane_index(),
        inactive,
        active,
    )
}

fn screen_with(bytes: &[u8], size: TerminalSize) -> Screen {
    let mut screen = Screen::new(size, 100);
    let mut parser = InputParser::new();
    parser.parse(bytes, &mut screen);
    screen
}

fn visible_line_text(screen: &Screen, row: usize, cols: usize) -> String {
    let mut text = String::new();
    assert!(screen.visit_visible_line_cells(row, cols, |cell| text.push_str(cell.text())));
    text
}

/// The full-width visible text of `row` after replaying `frame` onto a `size` screen.
fn frame_row(frame: &[u8], size: TerminalSize, row: usize) -> String {
    visible_line_text(&screen_with(frame, size), row, usize::from(size.cols))
}

/// Global replacements of `entries`, then blank `window-status-format` and
/// `window-status-current-format`.
fn status_options<'a>(entries: impl IntoIterator<Item = (OptionName, &'a str)>) -> OptionStore {
    option_store(
        entries
            .into_iter()
            .chain([
                (OptionName::WindowStatusFormat, ""),
                (OptionName::WindowStatusCurrentFormat, ""),
            ])
            .map(|(option, value)| (ScopeSelector::Global, option, value, SetOptionMode::Replace)),
    )
}

fn set_session_option_by_name(
    options: &mut OptionStore,
    session: &Session,
    name: &str,
    value: impl Into<String>,
) {
    options
        .set_by_name(
            rmux_proto::types::OptionScopeSelector::Session(session.name().clone()),
            name,
            Some(value.into()),
            SetOptionMode::Replace,
            false,
            false,
            false,
        )
        .expect("status-format option set succeeds");
}

fn render_text(session: &Session, options: &OptionStore) -> String {
    String::from_utf8(render(session, options)).expect("frame is utf-8")
}

/// Renders pane 0 of `session` over a `size` screen fed `bytes`.
fn pane_zero_frame(
    session: &Session,
    options: &OptionStore,
    bytes: &[u8],
    size: TerminalSize,
) -> String {
    let pane = session.window().pane(0).expect("pane 0 exists");
    let screen = screen_with(bytes, size);
    String::from_utf8(super::render_pane_screen(session, options, pane, &screen))
        .expect("pane frame is utf-8")
}

/// Renders the only pane of a `size` `alpha` session over a `size` screen fed `bytes`.
fn single_pane_frame(size: TerminalSize, options: &OptionStore, bytes: &[u8]) -> String {
    pane_zero_frame(&alpha_session(size), options, bytes, size)
}

fn pane_cursor_frame(screen: &Screen) -> String {
    let session = alpha_session(TerminalSize::new(20, 4));
    let pane = session.active_pane().expect("active pane exists");
    String::from_utf8(super::render_pane_cursor(
        &session,
        &OptionStore::new(),
        pane,
        screen,
    ))
    .expect("cursor frame is utf-8")
}

fn status_text(
    session: &Session,
    options: &OptionStore,
    columns: u16,
    attached_count: usize,
) -> String {
    status_bar_runs(session, options, columns, attached_count)
        .into_iter()
        .map(|run| run.text)
        .collect()
}

fn status_message_frame(size: TerminalSize, message: &str) -> String {
    String::from_utf8(super::render_status_message(
        &alpha_session(size),
        &OptionStore::new(),
        message,
    ))
    .expect("status message frame is utf-8")
}

/// Renders the status line until `needle` appears, or fails on the deadline.
///
/// The state comes from a real [`RequestHandler`] rather than a bare `HandlerState::default()`,
/// because a `#()` status job is a managed workload through `ShellIo::execute`. A unit test has
/// no daemon to bind a facade, so the state builds its own engine lazily — and reaches it only
/// through the weak handler the handler's constructor registers. A standalone state has no
/// handler to reach, so it never gets an engine, the job never starts, and the needle could only
/// ever time out.
///
/// Async, and its callers are `multi_thread`, for the same reason: the engine needs a runtime to
/// own its job pumps, and the job has to make progress on a worker other than the one this loop
/// is sleeping on.
async fn render_until_contains(session: &Session, options: &OptionStore, needle: &str) -> String {
    let handler = RequestHandler::new();
    let deadline = std::time::Instant::now() + status_job_test_deadline();
    loop {
        let frame = {
            let state = handler.state_for_test().lock().await;
            String::from_utf8(super::render_with_attached_count_prompt_and_pane_title(
                session,
                options,
                0,
                super::StatusRenderContext {
                    state: Some(&state),
                    ..super::StatusRenderContext::default()
                },
            ))
            .expect("frame is utf-8")
        };
        assert!(!frame.contains("#("), "{frame}");
        if frame.contains(needle) {
            return frame;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "render never contained {needle:?}; last frame was {frame:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn status_job_test_deadline() -> std::time::Duration {
    std::time::Duration::from_secs(2)
}

fn copy_mode_summary_with_time(top_line_time: i64) -> CopyModeSummary {
    CopyModeSummary {
        view_mode: false,
        line_numbers_enabled: false,
        show_position: true,
        history_size: 1,
        backing_rows: 4,
        scroll_position: 0,
        rectangle_toggle: false,
        cursor_x: 0,
        cursor_y: 0,
        selection_start: None,
        selection_end: None,
        selection_active: false,
        selection_present: false,
        selection_mode: None,
        search_present: false,
        search_timed_out: false,
        search_count: 0,
        search_count_partial: false,
        search_match: None,
        copy_cursor_word: String::new(),
        copy_cursor_line: String::new(),
        copy_cursor_hyperlink: String::new(),
        pane_search_string: String::new(),
        top_line_time,
    }
}

/// Renders the copy-mode position badge for pane 0 of a `size` `alpha` session.
fn copy_mode_position_frame(
    size: TerminalSize,
    options: &OptionStore,
    summary: &CopyModeSummary,
    history_size: usize,
) -> String {
    let session = alpha_session(size);
    let pane = session.window().pane(0).expect("pane 0 exists");
    String::from_utf8(super::render_copy_mode_position(
        &session,
        options,
        0,
        pane,
        summary,
        history_size,
        false,
    ))
    .expect("copy-mode position frame is utf-8")
}

fn line_number_options() -> OptionStore {
    option_store([(
        ScopeSelector::Global,
        OptionName::CopyModeLineNumbers,
        "absolute",
        SetOptionMode::Replace,
    )])
}

fn assert_copy_mode_badge_starts_at_bracket(cols: u16, top_line_time: i64, separator: &str) {
    let frame = copy_mode_position_frame(
        TerminalSize::new(cols, 4),
        &OptionStore::new(),
        &copy_mode_summary_with_time(top_line_time),
        1,
    );

    assert!(
        frame.contains("\u{1b}[0;30;43m[0/1]") || frame.contains("\u{1b}[30;43m[0/1]"),
        "copy-mode badge should start styling at '[': {frame:?}"
    );
    assert!(
        !frame.contains("\u{1b}[0;30;43m [0/1]") && !frame.contains("\u{1b}[30;43m [0/1]"),
        "copy-mode badge must not paint {separator}: {frame:?}"
    );
}

#[test]
fn rendered_pane_line_truncates_to_pane_width_without_counting_sgr() {
    let utf8 = Utf8Config::default();
    let clipped = String::from_utf8(super::truncate_rendered_pane_line(
        b"\x1b[31mabcdef",
        3,
        &utf8,
    ))
    .expect("utf8");

    assert_eq!(clipped, "\x1b[31mabc");

    let clipped_wide = String::from_utf8(super::truncate_rendered_pane_line(
        "表ab".as_bytes(),
        3,
        &utf8,
    ))
    .expect("utf8");
    assert_eq!(clipped_wide, "表a");
}

#[test]
fn rendered_pane_line_closes_hyperlink_when_visible_text_is_clipped() {
    let utf8 = Utf8Config::default();
    let close = "\u{1b}]8;;\u{1b}\\";

    for (line, expected) in [
        (
            "\u{1b}]8;id=ascii;https://example.test\u{1b}\\AB\u{1b}]8;;\u{1b}\\",
            format!("\u{1b}]8;id=ascii;https://example.test\u{1b}\\A{close}"),
        ),
        (
            "\u{1b}]8;;https://example.test\u{7}表B\u{1b}]8;;\u{7}",
            format!("\u{1b}]8;;https://example.test\u{7}表{close}"),
        ),
    ] {
        let clipped = String::from_utf8(super::truncate_rendered_pane_line(
            line.as_bytes(),
            if line.contains('表') { 2 } else { 1 },
            &utf8,
        ))
        .expect("rendered pane line is utf-8");
        assert_eq!(clipped, expected);
    }
}

#[test]
fn rendered_pane_line_keeps_composed_cells_at_pane_width() {
    let utf8 = Utf8Config::default();

    for (line, expected) in [
        ("👋🏽ABC", "👋🏽ABC"),
        ("👩\u{200d}💻ABC", "👩\u{200d}💻ABC"),
        ("\u{1b}[31m👋🏽\u{1b}[32mABC", "\u{1b}[31m👋🏽\u{1b}[32mABC"),
    ] {
        let clipped = String::from_utf8(super::truncate_rendered_pane_line(
            line.as_bytes(),
            5,
            &utf8,
        ))
        .expect("rendered pane line is utf-8");

        assert_eq!(clipped, expected, "line {line:?}");
    }
}

#[test]
fn pane_render_keeps_modified_emoji_text_at_right_edge() {
    let frame = single_pane_frame(
        TerminalSize::new(5, 3),
        &OptionStore::new(),
        "👋🏽ABC".as_bytes(),
    );

    assert!(
        frame.contains("👋🏽ABC"),
        "full repaint must preserve the complete composed cell and following text: {frame:?}"
    );
}

#[test]
fn copy_mode_position_truncation_does_not_style_separator_before_bracket() {
    assert_copy_mode_badge_starts_at_bracket(6, 1, "the truncated separator space");
}

#[test]
fn copy_mode_position_without_time_does_not_style_separator_before_bracket() {
    assert_copy_mode_badge_starts_at_bracket(100, 0, "a leading separator when no time is shown");
}

#[test]
fn copy_mode_position_badge_stays_out_of_the_line_number_gutter() {
    let summary = CopyModeSummary {
        line_numbers_enabled: true,
        history_size: 0,
        backing_rows: 4,
        ..copy_mode_summary_with_time(0)
    };
    let frame =
        copy_mode_position_frame(TerminalSize::new(6, 4), &line_number_options(), &summary, 0);

    assert!(
        frame.contains("\u{1b}[1;5H"),
        "the two-column badge must start after the four-column gutter: {frame:?}"
    );
}

#[test]
fn copy_mode_position_uses_tmux_absolute_line_number_formats() {
    let size = TerminalSize::new(20, 10);
    let options = line_number_options();
    let mut summary = CopyModeSummary {
        line_numbers_enabled: true,
        history_size: 31,
        backing_rows: 10,
        scroll_position: 31,
        ..copy_mode_summary_with_time(0)
    };

    let absolute = copy_mode_position_frame(size, &options, &summary, 31);
    assert!(absolute.contains("[1/41]"), "absolute format: {absolute:?}");

    summary.line_numbers_enabled = false;
    let mouse_origin = copy_mode_position_frame(size, &options, &summary, 31);
    assert!(
        mouse_origin.contains("[31/31]"),
        "mouse-origin format: {mouse_origin:?}"
    );
}

#[test]
fn hidden_copy_mode_position_emits_no_badge() {
    let summary = CopyModeSummary {
        show_position: false,
        ..copy_mode_summary_with_time(0)
    };

    assert!(
        copy_mode_position_frame(TerminalSize::new(20, 4), &OptionStore::new(), &summary, 1,)
            .is_empty()
    );
}

#[test]
fn clipped_cursor_marker_is_repainted_after_position_badge() {
    let summary = CopyModeSummary {
        line_numbers_enabled: true,
        cursor_x: 7,
        ..copy_mode_summary_with_time(0)
    };

    let frame =
        copy_mode_position_frame(TerminalSize::new(8, 2), &line_number_options(), &summary, 1);
    assert!(
        frame.ends_with("\u{1b}[1;8H\u{1b}[0m$\u{1b}[0m"),
        "tmux paints '$' after the top-row badge: {frame:?}"
    );
}

fn has_cell(cells: &[super::BorderCell], x: u16, y: u16, glyph: char) -> bool {
    cells
        .iter()
        .any(|cell| cell.x == x && cell.y == y && cell.glyph == glyph)
}

fn has_styled_cell(
    cells: &[super::BorderCell],
    x: u16,
    y: u16,
    glyph: char,
    style: &BorderStyle,
) -> bool {
    cells
        .iter()
        .any(|cell| cell.x == x && cell.y == y && cell.glyph == glyph && &cell.style == style)
}

#[test]
fn style_parser_maps_supported_forms_to_exact_ansi_bytes() {
    assert_eq!(style_sgr_bytes(&border_style(None), false), b"\x1b[0m");
    assert_eq!(
        style_sgr_bytes(&border_style(Some("default")), false),
        b"\x1b[0m"
    );
    assert_eq!(
        style_sgr_bytes(&border_style(Some("colour214")), false),
        b"\x1b[38;5;214m"
    );

    for (value, sgr) in [
        ("black", b"\x1b[30m".as_slice()),
        ("red", b"\x1b[31m".as_slice()),
        ("green", b"\x1b[32m".as_slice()),
        ("yellow", b"\x1b[33m".as_slice()),
        ("blue", b"\x1b[34m".as_slice()),
        (concat!("mag", "enta"), b"\x1b[35m".as_slice()),
        ("cyan", b"\x1b[36m".as_slice()),
        ("white", b"\x1b[37m".as_slice()),
        ("brightblack", b"\x1b[90m".as_slice()),
        ("brightred", b"\x1b[91m".as_slice()),
        ("brightgreen", b"\x1b[92m".as_slice()),
        ("brightyellow", b"\x1b[93m".as_slice()),
        ("brightblue", b"\x1b[94m".as_slice()),
        (concat!("bright", "mag", "enta"), b"\x1b[95m".as_slice()),
        ("brightcyan", b"\x1b[96m".as_slice()),
        ("brightwhite", b"\x1b[97m".as_slice()),
    ] {
        assert_eq!(style_sgr_bytes(&border_style(Some(value)), false), sgr);
    }

    assert_eq!(
        style_sgr_bytes(&parse_standalone_style(Some("fg=red")), false),
        b"\x1b[31m"
    );
    assert_eq!(
        style_sgr_bytes(
            &parse_standalone_style(Some("bg=green,fg=black,bold,reverse")),
            false,
        ),
        b"\x1b[0;1;7;30;42m"
    );
    assert_eq!(
        style_sgr_bytes(
            &parse_standalone_style(Some("fg=colour214,bg=brightblue")),
            false
        ),
        b"\x1b[0;38;5;214;104m"
    );
}

#[test]
fn sessions_without_visible_borders_emit_status_only_when_enabled() {
    let session = alpha_session(TerminalSize::new(80, 24));
    assert!(window_border_cells(&session, Style::default(), Style::default()).is_empty());
    let default_frame = render_text(&session, &OptionStore::new());
    assert!(default_frame.contains("[alpha]"));
    assert!(!default_frame.contains('┬'));

    let status_off = option_store([(
        ScopeSelector::Session(session.name().clone()),
        OptionName::Status,
        "off",
        SetOptionMode::Replace,
    )]);
    assert!(render(&session, &status_off).is_empty());

    let mut narrow = Session::new(session_name("narrow"), TerminalSize { cols: 3, rows: 2 });
    narrow.split_active_pane().expect("split succeeds");
    narrow.resize_terminal(TerminalSize { cols: 1, rows: 2 });
    assert!(!render(&narrow, &OptionStore::new()).is_empty());

    let mut zero_height = Session::new(session_name("flat"), TerminalSize { cols: 80, rows: 3 });
    zero_height
        .split_active_pane_with_direction(SplitDirection::Horizontal)
        .expect("split succeeds");
    zero_height.resize_terminal(TerminalSize { cols: 80, rows: 0 });
    assert!(render(&zero_height, &OptionStore::new()).is_empty());
}

#[test]
fn zoomed_sessions_clear_before_redrawing_active_pane() {
    let mut session = split_alpha_session(TerminalSize::new(80, 24));
    session
        .resize_pane(0, ResizePaneAdjustment::Zoom)
        .expect("zoom succeeds");

    let frame = render(&session, &OptionStore::new());
    assert!(
        frame.starts_with(b"\x1b[0m\x1b[H\x1b[2J"),
        "zoom repaint must clear stale non-active pane cells before drawing"
    );
}

#[test]
fn zoomed_sessions_render_only_the_active_pane_screen() {
    let size = TerminalSize { cols: 20, rows: 6 };
    let mut session = split_alpha_session(size);
    session
        .resize_pane(0, ResizePaneAdjustment::Zoom)
        .expect("zoom succeeds");
    let options = OptionStore::new();
    let inactive_pane = session.window().pane(1).expect("pane 1 exists");

    let active_frame = pane_zero_frame(&session, &options, b"VISIBLE_LEFT", size);
    let inactive_frame = super::render_pane_screen(
        &session,
        &options,
        inactive_pane,
        &screen_with(b"HIDDEN_RIGHT", size),
    );

    assert!(active_frame.contains("VISIBLE_LEFT"), "{active_frame}");
    assert!(
        inactive_frame.is_empty(),
        "zoomed repaint must not draw non-active pane content"
    );
}

#[test]
fn pane_render_leaves_default_cells_at_terminal_default_without_user_style() {
    let frame = single_pane_frame(
        TerminalSize::new(6, 2),
        &OptionStore::new(),
        b"\x1b[44mB\x1b[0mD",
    );

    assert!(frame.contains("\u{1b}[44mB"), "{frame:?}");
    assert!(frame.contains("\u{1b}[49mD"), "{frame:?}");
    assert!(!frame.contains("\u{1b}[40mD"), "{frame:?}");
}

#[test]
fn pane_render_uses_line_clear_for_unstyled_full_width_panes() {
    let frame = single_pane_frame(TerminalSize::new(12, 3), &OptionStore::new(), b"short");

    assert!(
        frame.contains("\u{1b}[1;1H\u{1b}[0mshort\u{1b}[0m\u{1b}[K"),
        "{frame:?}"
    );
    assert!(frame.contains("\u{1b}[2;1H\u{1b}[0m\u{1b}[K"), "{frame:?}");
    assert!(
        !frame.contains("short       "),
        "full-width unstyled panes should clear trailing cells instead of padding: {frame:?}"
    );
}

#[test]
fn pane_selection_overlay_style_expands_defaults_and_overrides() {
    let session = alpha_session(TerminalSize::new(6, 2));
    let pane = session.window().pane(0).expect("pane 0 exists");
    let overlay_style = |options: &OptionStore| {
        super::pane_screen::pane_selection_overlay_style(&session, options, pane)
    };

    // Default: copy-mode-selection-style is "#{E:mode-style}", which must
    // expand through the format engine to the mode-style default instead of
    // reaching the cell style parser as a raw template (issue #90).
    let style = overlay_style(&OptionStore::new()).expect("default selection style expands");
    assert!(
        style.contains("fg=black") && style.contains("bg=yellow"),
        "default selection style must expand mode-style, got {style:?}"
    );

    // The default follows a changed mode-style.
    let style = overlay_style(&option_store([(
        ScopeSelector::Global,
        OptionName::ModeStyle,
        "bg=blue,fg=white",
        SetOptionMode::Replace,
    )]))
    .expect("inherited selection style expands");
    assert!(
        style.contains("bg=blue"),
        "selection style must follow mode-style, got {style:?}"
    );

    // An explicit copy-mode-selection-style wins over mode-style.
    let style = overlay_style(&option_store([
        (
            ScopeSelector::Global,
            OptionName::ModeStyle,
            "bg=blue,fg=white",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::CopyModeSelectionStyle,
            "bg=red",
            SetOptionMode::Replace,
        ),
    ]))
    .expect("explicit selection style expands");
    assert!(
        style.contains("bg=red") && !style.contains("bg=blue"),
        "explicit selection style must win, got {style:?}"
    );
}

#[test]
fn styled_pane_screen_borrows_when_no_overlay_is_needed() {
    let size = TerminalSize { cols: 6, rows: 2 };
    let session = alpha_session(size);
    let pane = session.window().pane(0).expect("pane 0 exists");
    let screen = screen_with(b"D", size);
    let options = OptionStore::new();

    assert!(matches!(
        super::styled_pane_screen(&session, &options, pane, &screen),
        std::borrow::Cow::Borrowed(_)
    ));
}

fn selected_cell_colours(
    options: &OptionStore,
) -> (rmux_core::input::Colour, rmux_core::input::Colour) {
    let size = TerminalSize { cols: 6, rows: 2 };
    let session = alpha_session(size);
    let pane = session.window().pane(0).expect("pane 0 exists");
    let mut screen = screen_with(b"D", size);
    screen.mark_selected_row_range(0, 0, 0);

    let styled = super::styled_pane_screen(&session, options, pane, &screen);
    let mut colours = None;
    assert!(styled.visit_visible_line_cells(0, 1, |cell| {
        colours = Some((cell.fg(), cell.bg()));
    }));
    colours.expect("selected cell exists")
}

#[test]
fn copy_mode_selection_style_default_expands_mode_style() {
    assert_eq!(selected_cell_colours(&OptionStore::new()), (0, 3));
}

#[test]
fn copy_mode_selection_style_tracks_mode_style_until_explicitly_overridden() {
    let mut options = option_store([(
        ScopeSelector::Global,
        OptionName::ModeStyle,
        "bg=magenta,fg=white",
        SetOptionMode::Replace,
    )]);
    assert_eq!(selected_cell_colours(&options), (7, 5));

    options
        .set(
            ScopeSelector::Global,
            OptionName::CopyModeSelectionStyle,
            "bg=cyan,fg=red".to_owned(),
            SetOptionMode::Replace,
        )
        .expect("option set succeeds");
    assert_eq!(selected_cell_colours(&options), (1, 6));
}

#[test]
fn attach_render_golden_normal_idle_pane_is_byte_stable() {
    assert_eq!(
        single_pane_frame(TerminalSize::new(6, 2), &OptionStore::new(), b"D"),
        "\x1b[s\x1b[?25l\x1b[0m\x1b[1;1H\x1b[0mD\x1b[0m\x1b[K\x1b[0m\x1b[u\x1b[1;2H\x1b[?25h"
    );
}

#[test]
fn attach_render_pane_screen_with_prompt_preserves_prompt_cursor() {
    let size = TerminalSize { cols: 6, rows: 2 };
    let session = alpha_session(size);
    let pane = session.window().pane(0).expect("pane 0 exists");
    let screen = screen_with(b"D", size);
    let options = OptionStore::new();

    assert_eq!(
        super::render_pane_screen_preserving_prompt_cursor(&session, &options, pane, &screen),
        b"\x1b[s\x1b[?25l\x1b[0m\x1b[1;1H\x1b[0mD\x1b[0m\x1b[K\x1b[0m\x1b[u\x1b[?25h"
    );
}

#[test]
fn pane_render_keeps_padding_for_split_panes_to_avoid_clearing_neighbors() {
    let size = TerminalSize { cols: 20, rows: 4 };
    let frame = pane_zero_frame(
        &split_alpha_session(size),
        &OptionStore::new(),
        b"left",
        size,
    );

    assert!(
        !frame.contains("\u{1b}[K"),
        "split-pane repaint must not clear to terminal EOL: {frame:?}"
    );
}

#[test]
fn pane_render_resets_before_default_split_pane_row_after_styled_row() {
    let size = TerminalSize { cols: 20, rows: 4 };
    let frame = pane_zero_frame(
        &split_alpha_session(size),
        &OptionStore::new(),
        b"\x1b[48;5;255m          \r\n\x1b[0mplain",
        size,
    );

    assert!(
        frame.contains("\u{1b}[1;1H\u{1b}[0m\u{1b}[48;5;255m"),
        "{frame:?}"
    );
    assert!(
        frame.contains("\u{1b}[2;1H\u{1b}[0mplain"),
        "default rows must not inherit the previous row's background: {frame:?}"
    );
    assert!(
        !frame.contains("\u{1b}[K"),
        "split-pane repaint must still avoid clearing neighboring columns: {frame:?}"
    );
}

#[test]
fn pane_render_applies_window_style_to_default_cells() {
    let window = WindowTarget::with_window(session_name("alpha"), 0);
    let options = option_store([(
        ScopeSelector::Window(window),
        OptionName::WindowStyle,
        "bg=black",
        SetOptionMode::Replace,
    )]);

    let frame = single_pane_frame(TerminalSize::new(6, 2), &options, b"\x1b[44mB\x1b[0mD");

    assert!(frame.contains("\u{1b}[44mB"), "{frame:?}");
    assert!(frame.contains("\u{1b}[40mD"), "{frame:?}");
    assert!(
        frame.contains("\u{1b}[40mD    "),
        "styled default cells must still fill the pane background: {frame:?}"
    );
}

#[test]
fn pane_render_active_style_overlays_window_style_for_default_cells() {
    let window = WindowTarget::with_window(session_name("alpha"), 0);
    let options = option_store([
        (
            ScopeSelector::Window(window.clone()),
            OptionName::WindowStyle,
            "bg=black",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Window(window),
            OptionName::WindowActiveStyle,
            "bg=red",
            SetOptionMode::Replace,
        ),
    ]);

    let frame = single_pane_frame(TerminalSize::new(6, 2), &options, b"D");

    assert!(frame.contains("\u{1b}[41mD"), "{frame:?}");
}

#[test]
fn two_pane_sessions_render_the_main_vertical_border_column_and_exact_frame_bytes() {
    let session = split_alpha_session(TerminalSize::new(4, 2));
    let cells = window_border_cells(
        &session,
        border_style(Some("red")),
        border_style(Some("red")),
    );

    assert!(has_cell(&cells, 2, 0, '│'));
    assert!(has_cell(&cells, 2, 1, '│'));
    assert_eq!(cells.len(), 2);
    assert_eq!(
        super::render_cells(&cells),
        b"\x1b[s\x1b[0m\x1b[1;3H\x1b[31m\xe2\x94\x82\x1b[2;3H\xe2\x94\x82\x1b[0m\x1b[u"
    );
}

#[test]
fn two_pane_sessions_colour_only_the_active_half_of_the_shared_border() {
    let session = split_alpha_session(TerminalSize::new(10, 4));
    let inactive = border_style(Some("blue"));
    let active = border_style(Some("red"));
    let cells = window_border_cells(&session, inactive.clone(), active.clone());

    assert!(has_styled_cell(&cells, 5, 0, '│', &inactive));
    assert!(has_styled_cell(&cells, 5, 1, '│', &inactive));
    assert!(has_styled_cell(&cells, 5, 3, '│', &active));
}

#[test]
fn three_pane_sessions_render_full_height_vertical_dividers() {
    let session = session_with_three_panes();
    let cells = window_border_cells(&session, Style::default(), Style::default());

    for (x, y) in [(40, 0), (40, 12), (60, 0), (60, 12), (60, 23)] {
        assert!(has_cell(&cells, x, y, '│'));
    }
}

#[test]
fn four_pane_sessions_keep_vertical_splits_as_full_height_bars() {
    let mut session = session_with_three_panes();
    session.split_pane(2).expect("third split succeeds");
    let cells = window_border_cells(&session, Style::default(), Style::default());

    assert_eq!(
        cells.iter().filter(|cell| cell.glyph == '┬').count(),
        0,
        "parallel vertical splits should not sprout top tees at the screen edge"
    );
    assert_eq!(
        cells.iter().filter(|cell| cell.glyph == '┴').count(),
        0,
        "parallel vertical splits should not sprout bottom tees above the status line"
    );
}

#[test]
fn lower_vertical_split_joins_top_bottom_border_with_a_top_tee() {
    let mut session = alpha_session(TerminalSize::new(80, 24));
    let bottom = session
        .split_active_pane_with_direction(SplitDirection::Horizontal)
        .expect("horizontal split succeeds");
    session
        .split_pane_with_direction(bottom, SplitDirection::Vertical)
        .expect("vertical split succeeds");
    let cells = window_border_cells(&session, Style::default(), Style::default());

    let top_geometry = session
        .window()
        .pane(0)
        .expect("top pane exists")
        .geometry();
    let lower_left_geometry = session
        .window()
        .pane(bottom)
        .expect("lower-left pane exists")
        .geometry();
    let junction_x = lower_left_geometry
        .x()
        .saturating_add(lower_left_geometry.cols());
    let junction_y = top_geometry.y().saturating_add(top_geometry.rows());

    assert!(has_cell(&cells, junction_x, junction_y, '┬'));
    assert!(!has_cell(&cells, junction_x, junction_y, '┼'));
    assert!(!has_cell(
        &cells,
        junction_x,
        junction_y.saturating_sub(1),
        '│'
    ));
}

#[test]
fn active_and_inactive_styles_follow_the_active_pane_border_segments() {
    let mut session = session_with_three_panes();
    session.select_pane(0).expect("pane selection succeeds");
    let active = border_style(Some("red"));
    let inactive = border_style(Some("blue"));
    let cells = window_border_cells(&session, inactive.clone(), active.clone());

    assert!(has_styled_cell(&cells, 40, 18, '│', &active));
    assert!(has_styled_cell(&cells, 60, 6, '│', &inactive));
    assert!(has_styled_cell(&cells, 60, 23, '│', &inactive));
    assert!(has_styled_cell(&cells, 40, 23, '│', &active));
    assert!(!cells.iter().any(|cell| cell.y == 12 && cell.glyph == '─'));
}

#[test]
fn renderer_uses_session_option_resolution_and_renders_status_when_enabled() {
    let mut session = session_with_three_panes();
    session.select_pane(0).expect("pane selection succeeds");
    let session_name = session.name().clone();
    let window = WindowTarget::with_window(session_name.clone(), 0);
    let mut options = option_store([
        (
            ScopeSelector::Global,
            OptionName::PaneBorderStyle,
            "blue",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Window(window.clone()),
            OptionName::PaneBorderStyle,
            "yellow",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Window(window),
            OptionName::PaneActiveBorderStyle,
            "colour196",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Session(session_name.clone()),
            OptionName::Status,
            "off",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Session(session_name.clone()),
            OptionName::StatusLeft,
            "status #{session_name}",
            SetOptionMode::Replace,
        ),
    ]);

    let frame = render(&session, &options);
    let frame_text = String::from_utf8_lossy(&frame);

    assert!(frame_text.contains("\u{1b}[33m"));
    assert!(frame_text.contains("\u{1b}[38;5;196m"));
    assert!(frame_text.contains('│'));
    assert!(!frame_text.contains('┬'));
    assert!(!frame_text.contains('┴'));
    assert!(!frame_text.contains("status"));

    options
        .set(
            ScopeSelector::Session(session_name),
            OptionName::Status,
            "on".to_owned(),
            SetOptionMode::Replace,
        )
        .expect("option set succeeds");
    let status_frame = render(&session, &options);
    let status_text = String::from_utf8_lossy(&status_frame);
    assert!(status_text.contains("status al"));
    assert!(status_text.contains("\u{1b}[24;1H"));
}

#[test]
fn renderer_applies_pane_border_line_style() {
    let session = session_with_three_panes();
    let session_name = session.name().clone();
    let options = option_store([
        (
            ScopeSelector::Session(session_name.clone()),
            OptionName::Status,
            "off",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Window(WindowTarget::with_window(session_name, 0)),
            OptionName::PaneBorderLines,
            "heavy",
            SetOptionMode::Replace,
        ),
    ]);

    let frame = render_text(&session, &options);

    assert!(frame.contains('┃'), "{frame:?}");
    assert!(!frame.contains('│'), "{frame:?}");
}

#[test]
fn top_status_reserves_the_first_row_and_offsets_border_cells() {
    let session = session_with_three_panes();
    let options = option_store([(
        ScopeSelector::Session(session.name().clone()),
        OptionName::StatusPosition,
        "top",
        SetOptionMode::Replace,
    )]);

    let frame = render_text(&session, &options);

    assert!(frame.contains("\u{1b}[1;1H"));
    assert!(frame.contains("\u{1b}[2;41H"));
    assert!(!frame.contains("\u{1b}[1;41H┬"));
}

#[test]
fn status_window_list_uses_expanded_truncation_justify_and_raw_flags() {
    let size = TerminalSize { cols: 20, rows: 4 };
    let mut session = alpha_session(size);
    session
        .insert_window_with_initial_pane(1, size)
        .expect("window 1 insert succeeds");
    session
        .insert_window_with_initial_pane(2, size)
        .expect("window 2 insert succeeds");
    session.select_window(2).expect("window 2 select succeeds");
    session.select_window(1).expect("window 1 select succeeds");
    let options = option_store([
        (
            ScopeSelector::Global,
            OptionName::StatusStyle,
            "default",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::StatusLeft,
            "L#{session_name}LONG",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::StatusLeftLength,
            "4",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::StatusRight,
            "R#{session_windows}",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::StatusRightLength,
            "2",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::StatusJustify,
            "right",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::WindowStatusFormat,
            "#{window_index}#{window_raw_flags}",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::WindowStatusCurrentFormat,
            "#{window_index}#{window_raw_flags}",
            SetOptionMode::Replace,
        ),
    ]);

    let frame = render_text(&session, &options);

    assert!(frame.contains("Lalp"), "{frame}");
    assert!(frame.contains("1*"), "{frame}");
    assert!(frame.contains("R3"), "{frame}");
}

#[test]
fn status_format_override_replaces_default_status_line() {
    let session = alpha_session(TerminalSize::new(20, 3));
    let mut options = OptionStore::new();
    set_session_option_by_name(
        &mut options,
        &session,
        "status-format[0]",
        "custom #{session_name}",
    );

    let frame = render_text(&session, &options);

    assert!(frame.contains("custom alpha"), "{frame}");
    assert!(!frame.contains("0:zsh"), "{frame}");
}

#[test]
fn status_numeric_value_reserves_and_renders_multiple_status_lines() {
    let size = TerminalSize { cols: 20, rows: 6 };
    let session = alpha_session(size);
    let mut options = option_store([(
        ScopeSelector::Global,
        OptionName::Status,
        "3",
        SetOptionMode::Replace,
    )]);
    for (name, value) in [
        ("status-format[0]", "ZERO"),
        ("status-format[1]", "ONE"),
        ("status-format[2]", "TWO"),
    ] {
        set_session_option_by_name(&mut options, &session, name, value);
    }

    let screen = screen_with(&render(&session, &options), size);

    for (row, expected) in [(3, "ZERO"), (4, "ONE"), (5, "TWO")] {
        assert_eq!(
            visible_line_text(&screen, row, usize::from(size.cols)).trim_end(),
            expected
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn status_left_expands_shell_job() {
    let session = alpha_session(TerminalSize::new(80, 3));
    let marker = format!("statusjob{}", std::process::id());
    let command = format!("#(echo {marker})");
    let options = status_options([
        (OptionName::StatusLeft, command.as_str()),
        (OptionName::StatusLeftLength, "32"),
        (OptionName::StatusRight, ""),
    ]);

    let frame = render_until_contains(&session, &options, &marker).await;
    assert!(frame.contains(&marker), "{frame}");
}

#[tokio::test(flavor = "multi_thread")]
async fn status_format_expands_shell_job() {
    let session = alpha_session(TerminalSize::new(80, 3));
    let mut options = OptionStore::new();
    let marker = format!("statusformatjob{}", std::process::id());
    set_session_option_by_name(
        &mut options,
        &session,
        "status-format[0]",
        format!("#(echo {marker})"),
    );

    let frame = render_until_contains(&session, &options, &marker).await;
    assert!(frame.contains(&marker), "{frame}");
}

#[tokio::test(flavor = "multi_thread")]
async fn status_format_expands_shell_job_introduced_by_status_left() {
    let session = alpha_session(TerminalSize::new(80, 3));
    let marker = format!("statusleftjob{}", std::process::id());
    let status_left = format!("X#(echo {marker})Y");
    let options = option_store([
        (
            ScopeSelector::Global,
            OptionName::StatusFormat,
            "#{T:status-left}",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::StatusLeft,
            status_left.as_str(),
            SetOptionMode::Replace,
        ),
    ]);

    let frame = render_until_contains(&session, &options, &marker).await;
    assert!(frame.contains(&format!("X{marker}Y")), "{frame}");
}

#[test]
fn status_right_inline_styles_do_not_consume_length_budget() {
    let size = TerminalSize { cols: 20, rows: 3 };
    let options = status_options([
        (OptionName::StatusLeft, ""),
        (
            OptionName::StatusRight,
            "#[fg=#{?session_attached,green,red},bold]CLOCK-DATE-HOST",
        ),
        (OptionName::StatusRightLength, "10"),
    ]);

    let status = frame_row(&render(&alpha_session(size), &options), size, 2);

    assert_eq!(status, "          CLOCK-DATE", "{status:?}");
}

#[test]
fn explicit_status_format_width_modifier_ignores_inline_styles() {
    let size = TerminalSize { cols: 20, rows: 3 };
    let options = option_store([
        (
            ScopeSelector::Global,
            OptionName::StatusFormat,
            "#[align=right]#{T;=/10:status-right}",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::StatusRight,
            "#[fg=green]CLOCK-DATE-HOST",
            SetOptionMode::Replace,
        ),
    ]);

    let status = frame_row(&render(&alpha_session(size), &options), size, 2);

    assert_eq!(status, "          CLOCK-DATE", "{status:?}");
}

#[test]
fn status_left_inline_styles_preserve_unicode_cell_truncation() {
    let size = TerminalSize { cols: 12, rows: 3 };
    let options = status_options([
        (OptionName::StatusLeft, "#[fg=red]表A#[bold]👋🏽B"),
        (OptionName::StatusLeftLength, "5"),
        (OptionName::StatusRight, ""),
    ]);

    let status = frame_row(&render(&alpha_session(size), &options), size, 2);

    // Screen visitors expose each wide glyph's continuation cell as a space.
    assert_eq!(status, "表 A👋🏽        ", "{status:?}");
}

#[test]
fn status_component_limit_keeps_a_zwj_grapheme_whole_product_divergence() {
    let session = alpha_session(TerminalSize::new(8, 3));
    let options = status_options([
        (OptionName::StatusLeft, "#[fg=red]👩\u{200d}💻A"),
        (OptionName::StatusLeftLength, "2"),
        (OptionName::StatusRight, ""),
    ]);

    let frame = render_text(&session, &options);
    assert!(
        frame.contains("👩\u{200d}💻"),
        "the ZWJ grapheme must survive the formatted frame"
    );
    assert!(!frame.contains("👩\u{200d}💻A"), "{frame:?}");
}

#[test]
fn status_fill_applies_background_when_text_background_is_default() {
    let session = alpha_session(TerminalSize::new(8, 2));
    let options = status_options([
        (OptionName::StatusStyle, "fill=blue"),
        (OptionName::StatusLeft, "X"),
        (OptionName::StatusRight, ""),
    ]);

    let frame = render_text(&session, &options);
    assert!(frame.contains("\u{1b}[44m"));
}

#[test]
fn status_only_render_starts_from_a_reset_sgr_state() {
    let session = alpha_session(TerminalSize::new(8, 2));

    let frame = render_text(&session, &OptionStore::new());
    assert!(frame.starts_with("\u{1b}7\u{1b}[0m"));
}

#[test]
fn prompt_status_render_positions_cursor_on_the_input_cell() {
    let session = alpha_session(TerminalSize::new(20, 4));
    let prompt = super::RenderedPrompt {
        prompt: "rename-window ".to_owned(),
        input: String::new(),
        cursor: 0,
        command_prompt: false,
    };

    let frame = String::from_utf8(super::render_with_attached_count_and_prompt(
        &session,
        &OptionStore::new(),
        1,
        Some(&prompt),
    ))
    .expect("frame is utf-8");

    assert!(
        frame.ends_with("\u{1b}[4;15H"),
        "prompt cursor should land after the prompt label, got {frame:?}"
    );
}

#[test]
fn pane_cursor_render_repositions_and_shows_the_terminal_cursor() {
    let screen = screen_with(b"abc", TerminalSize { cols: 20, rows: 3 });

    assert_eq!(pane_cursor_frame(&screen), "\u{1b}[1;4H\u{1b}[?25h");
}

#[test]
fn pane_cursor_render_hides_terminal_cursor_when_screen_cursor_is_hidden() {
    let screen = screen_with(b"\x1b[?25l", TerminalSize { cols: 20, rows: 3 });
    assert_eq!(screen.mode() & mode::MODE_CURSOR, 0);

    assert_eq!(pane_cursor_frame(&screen), "\u{1b}[1;1H\u{1b}[?25l");
}

#[test]
fn border_render_starts_from_a_reset_sgr_state() {
    let session = split_alpha_session(TerminalSize::new(8, 4));

    let frame = render_text(&session, &OptionStore::new());
    assert!(frame.starts_with("\u{1b}[s\u{1b}[0m"));
}

#[test]
fn status_bar_runs_include_session_attached_in_status_context() {
    let session = alpha_session(TerminalSize::new(4, 2));
    let options = status_options([
        (OptionName::StatusLeft, "#{session_attached}"),
        (OptionName::StatusRight, ""),
    ]);

    assert_eq!(status_text(&session, &options, 4, 1), "1   ");
    assert_eq!(status_text(&session, &options, 4, 0), "0   ");
}

#[test]
fn status_message_text_cannot_emit_control_characters_into_the_status_row() {
    let frame = status_message_frame(TerminalSize::new(20, 4), "hi\nthere\t\x1b[31m");

    assert!(!frame.contains('\n'));
    assert!(!frame.contains('\t'));
    assert!(frame.contains("hi there  [31m"));
}

#[test]
fn status_message_renders_default_message_style_from_message_format() {
    let frame = status_message_frame(TerminalSize::new(20, 4), "No next window");

    assert!(
        frame.contains("\x1b[0;30;43m") || frame.contains("\x1b[30;43m"),
        "default message-format should expand message-style inside the style clause, got {frame:?}"
    );
}

#[test]
fn status_message_style_fills_the_full_status_line() {
    let frame = status_message_frame(TerminalSize::new(20, 4), "No next window");

    assert!(
        frame.contains("\x1b[0;30;43mNo next window      \x1b[0m")
            || frame.contains("\x1b[30;43mNo next window      \x1b[0m"),
        "message-style should fill the whole status row, got {frame:?}"
    );
}

#[test]
fn status_message_uses_message_line_with_multiline_status() {
    let size = TerminalSize { cols: 20, rows: 5 };
    let options = option_store([
        (
            ScopeSelector::Global,
            OptionName::Status,
            "2",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::MessageLine,
            "1",
            SetOptionMode::Replace,
        ),
    ]);

    let frame = super::render_status_message(&alpha_session(size), &options, "line-one");
    let screen = screen_with(&frame, size);

    assert_eq!(visible_line_text(&screen, 3, 8), "        ");
    assert_eq!(visible_line_text(&screen, 4, 8), "line-one");
}

#[test]
fn status_message_uses_last_terminal_row_when_status_is_off() {
    // Oracle tmux 3.7b: disabling status changes the backing row from status
    // storage to pane content, but does not suppress the message overlay.
    let size = TerminalSize { cols: 20, rows: 5 };
    let options = option_store([(
        ScopeSelector::Global,
        OptionName::Status,
        "off",
        SetOptionMode::Replace,
    )]);

    let frame = super::render_status_message(&alpha_session(size), &options, "status-off");
    let screen = screen_with(&frame, size);

    for row in 0..4 {
        assert_eq!(visible_line_text(&screen, row, 10), "          ");
    }
    assert_eq!(visible_line_text(&screen, 4, 10), "status-off");
}

#[test]
fn status_message_truncates_by_display_width_instead_of_scalar_count() {
    let frame = status_message_frame(TerminalSize::new(3, 4), "表ab");

    assert!(frame.contains("表a"));
    assert!(!frame.contains("表ab"));
}

#[test]
fn status_bar_spacing_uses_display_width_for_cjk_and_emoji() {
    let session = alpha_session(TerminalSize::new(6, 4));
    let options = status_options([
        (OptionName::StatusLeft, "表A"),
        (OptionName::StatusRight, "🇨🇭"),
    ]);

    assert_eq!(status_text(&session, &options, 6, 0), "表A 🇨🇭");
}

#[test]
fn pane_active_border_style_conditionals_are_runtime_expanded() {
    let session = split_alpha_session(TerminalSize::new(10, 4));
    let window = WindowTarget::with_window(session.name().clone(), 0);
    let options = option_store([
        (
            ScopeSelector::Window(window.clone()),
            OptionName::PaneBorderStyle,
            "green",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Window(window),
            OptionName::PaneActiveBorderStyle,
            "#{?pane_active,red,blue}",
            SetOptionMode::Replace,
        ),
    ]);

    let frame = render(&session, &options);
    let frame_text = String::from_utf8_lossy(&frame);

    assert!(frame_text.contains("\u{1b}[32m"));
    assert!(frame_text.contains("\u{1b}[31m"));
    assert!(!frame_text.contains("\u{1b}[34m"));
}
