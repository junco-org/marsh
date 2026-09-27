use super::{
    ClientPathUpdate, ClientTitleState, ClientTitleUpdate, CursorScope, OuterTerminal,
    OuterTerminalContext, RenderedClientTitle,
};
use rmux_core::input::mode::{MODE_MOUSE_ALL, MODE_MOUSE_BUTTON};
use rmux_core::{OptionStore, Session};
use rmux_proto::{
    ClientTerminalContext, OptionName, ScopeSelector, SetOptionMode, TerminalSize, WindowTarget,
};

use crate::test_fixtures::option_store;
use crate::test_names::session_name;

const MOUSE_ENABLE_SEQUENCE: &str = "\u{1b}[?1006h\u{1b}[?1000h\u{1b}[?1002h";
const MOUSE_DISABLE_SEQUENCE: &str = "\u{1b}[?1002l\u{1b}[?1000l\u{1b}[?1006l";
const MOUSE_ENABLE_PARTS: [&str; 4] = [
    "\u{1b}[?1006h",
    "\u{1b}[?1002h",
    "\u{1b}[?1000h",
    MOUSE_ENABLE_SEQUENCE,
];
const MOUSE_DISABLE_PARTS: [&str; 4] = [
    "\u{1b}[?1000l",
    "\u{1b}[?1002l",
    "\u{1b}[?1006l",
    MOUSE_DISABLE_SEQUENCE,
];
const TITLE_PANE_PATH: &str = "file:///tmp/project";

fn resolve_term(options: &OptionStore, term: &str) -> OuterTerminal {
    OuterTerminal::resolve(options, OuterTerminalContext::from_pairs(&[("TERM", term)]))
}

fn xterm_with<'a>(
    settings: impl IntoIterator<Item = (OptionName, &'a str, SetOptionMode)>,
) -> OuterTerminal {
    let settings = settings
        .into_iter()
        .map(|(option, value, mode)| (ScopeSelector::Global, option, value, mode));
    resolve_term(&option_store(settings), "xterm-256color")
}

fn xterm_context() -> OuterTerminalContext {
    OuterTerminalContext::from_pairs(&[("TERM", "xterm-256color")])
}

fn mouse_client_context() -> OuterTerminalContext {
    OuterTerminalContext::default().with_client_terminal(&ClientTerminalContext {
        terminal_features: vec!["mouse".to_owned()],
        utf8: true,
    })
}

fn resolve_alpha(options: &OptionStore, context: OuterTerminalContext) -> OuterTerminal {
    OuterTerminal::resolve_for_session(options, Some(&session_name("alpha")), context)
}

fn utf8(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).expect("utf8")
}

fn assert_contains_all(haystack: &str, needles: &[&str]) {
    for needle in needles {
        assert!(
            haystack.contains(needle),
            "{needle:?} missing from {haystack:?}"
        );
    }
}

/// The options and outer terminal behind a title render.
struct TitleFixture {
    options: OptionStore,
    terminal: OuterTerminal,
}

impl TitleFixture {
    /// A terminal advertising both the title and OSC 7 templates, so what a render
    /// records is what it would really put on the wire.
    fn capable() -> Self {
        Self::capable_with(|_| {})
    }

    fn capable_with(configure: impl FnOnce(&mut OptionStore)) -> Self {
        let mut options = option_store([(
            ScopeSelector::Global,
            OptionName::TerminalFeatures,
            "tmux*:osc7",
            SetOptionMode::Append,
        )]);
        configure(&mut options);
        let terminal = resolve_term(&options, "tmux-256color");
        Self { options, terminal }
    }

    /// No TERM at all: the issue #182 case, where no terminal family and no XT flag
    /// supply a title capability.
    fn incapable() -> Self {
        let options = OptionStore::new();
        let terminal = OuterTerminal::resolve(&options, OuterTerminalContext::default());
        Self { options, terminal }
    }

    fn prelude(&self, update: ClientTitleUpdate<'_>) -> String {
        let session = Session::new(session_name("alpha"), TerminalSize { cols: 80, rows: 24 });
        utf8(
            self.terminal
                .render_prelude(&session, &self.options, CursorScope::Pane, update),
        )
    }

    fn commit(&self, update: ClientTitleUpdate<'_>) -> RenderedClientTitle {
        update
            .rendered_by(&self.terminal)
            .expect("set-titles on commits")
    }
}

fn title_update<'a>(
    resolved: Option<&'a str>,
    path: &'a str,
    previous: Option<&'a ClientTitleState>,
) -> ClientTitleUpdate<'a> {
    ClientTitleUpdate {
        resolved,
        path: ClientPathUpdate::Reported(path),
        previous,
    }
}

fn shown_title(title: &str) -> ClientTitleState {
    ClientTitleState {
        title: Some(title.to_owned()),
        path: Some(TITLE_PANE_PATH.to_owned()),
    }
}

#[test]
fn terminal_features_match_globs_and_case_insensitive_feature_names() {
    let options = option_store([(
        ScopeSelector::Global,
        OptionName::TerminalFeatures,
        "xterm-kitty*:ClIpBoArD:EXTKEYS",
        SetOptionMode::Append,
    )]);
    let terminal = resolve_term(&options, "xterm-kitty");

    assert!(terminal.features_string().contains("clipboard"));
    assert!(terminal.features_string().contains("extkeys"));
}

#[test]
fn xterm_kitty_enables_kitty_graphics_feature() {
    let terminal = resolve_term(&OptionStore::default(), "xterm-kitty");

    assert!(terminal.supports_kitty_graphics());
    assert!(terminal.features_string().contains("kitty-graphics"));
}

#[test]
fn modern_kitty_graphics_terminals_enable_kitty_graphics_feature() {
    for context in [
        OuterTerminalContext::from_pairs(&[("TERM", "xterm-ghostty")]),
        OuterTerminalContext::from_pairs(&[("TERM", "wezterm")]),
        OuterTerminalContext::from_pairs(&[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", "ghostty"),
        ]),
        OuterTerminalContext::from_pairs(&[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", "WezTerm"),
        ]),
    ] {
        let terminal = OuterTerminal::resolve(&OptionStore::default(), context);
        assert!(terminal.supports_kitty_graphics());
        assert!(terminal.features_string().contains("kitty-graphics"));
    }
}

#[test]
fn known_sixel_terminals_enable_sixel_feature() {
    for context in [
        OuterTerminalContext::from_pairs(&[("TERM", "mintty")]),
        OuterTerminalContext::from_pairs(&[("TERM", "foot")]),
        OuterTerminalContext::from_pairs(&[("TERM", "mlterm")]),
        OuterTerminalContext::from_pairs(&[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", "WezTerm"),
        ]),
    ] {
        let terminal = OuterTerminal::resolve(&OptionStore::default(), context);
        assert!(terminal.supports_sixel());
        assert!(terminal.features_string().contains("sixel"));
    }
}

#[test]
fn terminal_features_can_enable_sixel_for_other_terms() {
    let terminal = xterm_with([(
        OptionName::TerminalFeatures,
        "xterm*:sixel",
        SetOptionMode::Append,
    )]);

    assert!(terminal.supports_sixel());
}

#[test]
fn terminal_overrides_apply_legacy_tc_xt_and_ax_flags() {
    let options = option_store([(
        ScopeSelector::Global,
        OptionName::TerminalOverrides,
        "linux*:Tc:XT:AX@",
        SetOptionMode::Append,
    )]);
    let terminal = resolve_term(&options, "linux");

    let features = terminal.features_string();
    assert!(features.contains("RGB"));
    assert!(features.contains("bpaste"));
    assert!(features.contains("focus"));
    assert!(features.contains("title"));
}

#[test]
fn attach_sequences_follow_focus_and_extended_key_options() {
    let options = option_store([
        (
            ScopeSelector::Global,
            OptionName::FocusEvents,
            "on",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::ExtendedKeys,
            "always",
            SetOptionMode::Replace,
        ),
        (
            ScopeSelector::Global,
            OptionName::Mouse,
            "on",
            SetOptionMode::Replace,
        ),
    ]);
    let terminal = resolve_alpha(
        &options,
        OuterTerminalContext::from_pairs(&[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]),
    );

    let start = utf8(terminal.attach_start_sequence());
    let stop = utf8(terminal.attach_stop_sequence());

    assert!(start.starts_with("\u{1b}[?1049h\u{1b}[22;0;0t\u{1b}[0m\u{1b}[?25l\u{1b}[H\u{1b}[2J"));
    assert_contains_all(
        &start,
        &[
            "\u{1b}[22;0;0t",
            "\u{1b}[?2004h",
            "\u{1b}[?1004h",
            "\u{1b}[>4;2m",
        ],
    );
    assert_contains_all(&start, &MOUSE_ENABLE_PARTS);
    assert_contains_all(&stop, &["\u{1b}[?2004l", "\u{1b}[?1004l", "\u{1b}[>4m"]);
    assert_contains_all(&stop, &MOUSE_DISABLE_PARTS);
    assert!(stop.ends_with("\u{1b}[?1049l\u{1b}[23;0;0t"));
}

#[test]
fn client_mouse_feature_enables_mouse_attach_sequences_when_mouse_option_is_on() {
    let options = option_store([(
        ScopeSelector::Global,
        OptionName::Mouse,
        "on",
        SetOptionMode::Replace,
    )]);
    let terminal = resolve_alpha(&options, mouse_client_context());

    assert_contains_all(&utf8(terminal.attach_start_sequence()), &MOUSE_ENABLE_PARTS);
    assert_contains_all(&utf8(terminal.attach_stop_sequence()), &MOUSE_DISABLE_PARTS);
}

#[test]
fn active_pane_mouse_tracking_enables_outer_mouse_with_mouse_option_off() {
    // Issue #93: tmux enables outer mouse reporting when the `mouse` option
    // is on OR the active pane's application requested a tracking mode, so
    // vim/htop over SSH must get mouse events with `mouse off`.
    let terminal = resolve_alpha(&OptionStore::new(), mouse_client_context())
        .with_active_pane_mouse_mode(MODE_MOUSE_BUTTON);

    let start = utf8(terminal.attach_start_sequence());
    assert!(
        start.contains(MOUSE_ENABLE_SEQUENCE),
        "pane-driven tracking must enable outer mouse despite mouse=off"
    );
}

#[test]
fn without_option_or_pane_tracking_outer_mouse_stays_disabled() {
    let terminal =
        resolve_alpha(&OptionStore::new(), mouse_client_context()).with_active_pane_mouse_mode(0);

    let start = utf8(terminal.attach_start_sequence());
    assert!(
        !start.contains("\u{1b}[?1000h"),
        "no option and no pane tracking must not enable outer mouse"
    );
}

#[test]
fn transition_disables_outer_mouse_when_the_pane_stops_tracking() {
    let tracking = resolve_alpha(&OptionStore::new(), mouse_client_context())
        .with_active_pane_mouse_mode(MODE_MOUSE_BUTTON);
    let idle =
        resolve_alpha(&OptionStore::new(), mouse_client_context()).with_active_pane_mouse_mode(0);

    let enable = utf8(tracking.transition_sequence_from(&idle));
    assert!(
        enable.contains(MOUSE_ENABLE_SEQUENCE),
        "pane starting to track must enable outer mouse on refresh"
    );
    let disable = utf8(idle.transition_sequence_from(&tracking));
    assert!(
        disable.contains(MOUSE_DISABLE_SEQUENCE),
        "pane resetting its tracking mode must disable outer mouse on refresh"
    );
}

#[test]
fn active_pane_all_motion_tracking_preserves_decset_1003() {
    let terminal = resolve_alpha(&OptionStore::new(), mouse_client_context())
        .with_active_pane_mouse_mode(MODE_MOUSE_ALL);

    let start = utf8(terminal.attach_start_sequence());
    assert!(start.contains("\u{1b}[?1003h"));
    assert!(!start.contains("\u{1b}[?1002h"));

    let stop = utf8(terminal.attach_stop_sequence());
    assert!(stop.contains("\u{1b}[?1003l"));
}

#[test]
fn focus_follows_mouse_option_upgrades_mouse_tracking_to_all_motion() {
    let mut options = option_store([(
        ScopeSelector::Global,
        OptionName::FocusFollowsMouse,
        "on",
        SetOptionMode::Replace,
    )]);

    let mouse_off = resolve_alpha(&options, mouse_client_context());
    let off_start = utf8(mouse_off.attach_start_sequence());
    assert!(!off_start.contains("\u{1b}[?1003h"));

    options
        .set(
            ScopeSelector::Global,
            OptionName::Mouse,
            "on".to_owned(),
            SetOptionMode::Replace,
        )
        .expect("global option set succeeds");
    let mouse_on = resolve_alpha(&options, mouse_client_context());
    let on_start = utf8(mouse_on.attach_start_sequence());
    assert!(on_start.contains("\u{1b}[?1003h"));
    assert!(!on_start.contains("\u{1b}[?1002h"));

    options
        .set(
            ScopeSelector::Global,
            OptionName::FocusFollowsMouse,
            "off".to_owned(),
            SetOptionMode::Replace,
        )
        .expect("global option set succeeds");
    let button_tracking = resolve_alpha(&options, mouse_client_context());
    let button_start = utf8(button_tracking.attach_start_sequence());
    assert!(button_start.contains("\u{1b}[?1002h"));
    assert!(!button_start.contains("\u{1b}[?1003h"));
}

/// The prelude's title/path gate, exercised straight against the capability
/// layer. `set-titles off` resolves no title, which on tmux 3.7b means the
/// attached client writes neither OSC 0 nor OSC 7 even when the terminal
/// advertises `title` and `osc7`.
#[test]
fn render_prelude_without_resolved_title_writes_neither_title_nor_path() {
    let fixture = TitleFixture::capable_with(|options| {
        options
            .set(
                ScopeSelector::Window(WindowTarget::with_window(session_name("alpha"), 0)),
                OptionName::CursorColour,
                "red".to_owned(),
                SetOptionMode::Replace,
            )
            .expect("cursor colour set succeeds");
    });

    let prelude = fixture.prelude(title_update(None, TITLE_PANE_PATH, None));

    assert!(
        !prelude.contains("\u{1b}]0;"),
        "set-titles off must not drive the outer title, got {prelude:?}"
    );
    assert!(
        !prelude.contains("\u{1b}]7;"),
        "set-titles off must not drive OSC 7 either, got {prelude:?}"
    );
    // The rest of the prelude is untouched by the title gate.
    assert!(prelude.contains("\u{1b}]12;rgb:cd/00/00\u{7}"));
}

/// A resolved title is the expanded `set-titles-string`, never the bare pane
/// title (issue #182), and it unlocks the neighbouring OSC 7 path.
#[test]
fn render_prelude_writes_resolved_title_and_path() {
    let update = title_update(Some("RMUXTEST alpha:0"), TITLE_PANE_PATH, None);
    let prelude = TitleFixture::capable().prelude(update);

    assert!(prelude.contains("\u{1b}]0;RMUXTEST alpha:0\u{7}"));
    assert!(prelude.contains("\u{1b}]7;file:///tmp/project\u{7}"));
}

/// tmux compares each expansion against `c->title` / `c->path` and writes only
/// what differs, so a refresh resolving the same values is silent. Measured on
/// tmux 3.7b: a static title and path survive four forced `refresh-client`
/// redraws with exactly one OSC 0 and one OSC 7 on the wire.
#[test]
fn render_prelude_skips_an_unchanged_title_and_path() {
    let fixture = TitleFixture::capable();
    let shown = shown_title("RMUXTEST alpha:0");

    let prelude = fixture.prelude(title_update(
        Some("RMUXTEST alpha:0"),
        TITLE_PANE_PATH,
        Some(&shown),
    ));
    assert!(
        !prelude.contains("\u{1b}]0;") && !prelude.contains("\u{1b}]7;"),
        "unchanged title and path must not be re-emitted, got {prelude:?}"
    );

    let changed = fixture.prelude(title_update(
        Some("RMUXTEST alpha:1"),
        "file:///tmp/other",
        Some(&shown),
    ));
    assert!(changed.contains("\u{1b}]0;RMUXTEST alpha:1\u{7}"));
    assert!(changed.contains("\u{1b}]7;file:///tmp/other\u{7}"));
}

/// tmux 3.7b writes the expanded title verbatim, so a title carrying `ESC ] 0 ;`
/// closes the sequence early and injects into the outer terminal. RMUX keeps
/// its deliberate divergence: control characters are neutralised before the
/// payload reaches the terminal.
#[test]
fn render_prelude_neutralises_control_characters_in_the_title() {
    let update = title_update(Some("A\u{1b}]0;INJECT\u{7}B\tC"), TITLE_PANE_PATH, None);
    let prelude = TitleFixture::capable().prelude(update);

    let title = prelude
        .split_once("\u{1b}]0;")
        .expect("a title is written")
        .1
        .split_once('\u{7}')
        .expect("the title terminates")
        .0;
    // ESC, BEL and TAB each become a space; the surviving "]0;" is inert text.
    assert_eq!(title, "A ]0;INJECT B C");
    assert!(
        !title.contains('\u{1b}') && !title.contains('\u{7}'),
        "no control character may survive into the title, got {title:?}"
    );
    // Exactly one OSC 0 introducer: the payload cannot open a second one.
    assert_eq!(prelude.matches("\u{1b}]0;").count(), 1);
}

/// A terminal that never advertised TSL/FSL keeps its title untouched, exactly
/// as tmux's `tty_set_title()` returns early without both capabilities. The
/// value is still remembered, matching tmux assigning `c->title` regardless.
#[test]
fn render_prelude_leaves_a_title_incapable_terminal_alone() {
    let fixture = TitleFixture::incapable();
    assert!(
        !fixture.terminal.features_string().contains("title"),
        "fixture must not advertise the title capability"
    );

    let update = title_update(Some("RMUXTEST alpha:0"), TITLE_PANE_PATH, None);
    let prelude = fixture.prelude(update);

    assert!(
        !prelude.contains("\u{1b}]0;") && !prelude.contains("RMUXTEST"),
        "a title-incapable terminal must receive no title, got {prelude:?}"
    );
    let rendered = fixture
        .terminal
        .rendered_client_title(update)
        .expect("set-titles on commits");
    assert_eq!(
        rendered.state().title(),
        Some("RMUXTEST alpha:0"),
        "tmux remembers c->title even when the tty cannot write it"
    );
    // Nothing reached the wire, so the frame stays replaceable in the control
    // queue: a client with no title template must not lose render coalescing
    // just because `set-titles` is on.
    assert!(
        !rendered.wrote(),
        "a terminal with no title template emits no OSC bytes"
    );
}

/// While `set-titles` is off nothing is committed, so the value the terminal
/// still shows survives the option going off and back on (oracle: tmux 3.7b
/// emits exactly one title across an on -> off -> on toggle).
#[test]
fn a_suppressed_title_commits_nothing_and_keeps_the_previous_path() {
    let fixture = TitleFixture::capable();
    let shown = shown_title("STABLE");
    assert!(title_update(None, "file:///tmp/other", Some(&shown))
        .rendered_by(&fixture.terminal)
        .is_none());

    // A render that resolves a title but reads no pane path keeps the path the
    // client was already given rather than forgetting it, and writes nothing.
    let rendered = fixture.commit(ClientTitleUpdate {
        resolved: Some("STABLE"),
        path: ClientPathUpdate::Unread,
        previous: Some(&shown),
    });
    assert_eq!(rendered.state(), &shown);
    assert!(
        !rendered.wrote(),
        "a fully deduplicated render puts no OSC bytes in the frame"
    );
}

/// A frame that actually carries OSC 0 / OSC 7 must stay in the control queue:
/// its successor deduplicates against it, so replacing it would strand the
/// outer terminal on the previous title with nothing left to correct it.
#[test]
fn a_title_carrying_render_is_not_a_replaceable_refresh() {
    let fixture = TitleFixture::capable();
    let shown = shown_title("STABLE");

    let wrote = fixture.commit(title_update(Some("CHANGED"), TITLE_PANE_PATH, Some(&shown)));
    assert!(wrote.wrote(), "a changed title puts OSC 0 in the frame");

    let path_only = fixture.commit(title_update(
        Some("STABLE"),
        "file:///tmp/other",
        Some(&shown),
    ));
    assert!(path_only.wrote(), "a changed path alone still writes OSC 7");
}

/// A render only commits to the client's remembered identity what it actually
/// put on the wire. The status tick expands under the state lock and commits
/// under the attach lock, so a concurrent refresh can deliver a different title
/// in between; adopting a value this render did not write would then claim the
/// outer terminal shows something it does not, and the next expansion back to
/// that value would be skipped — the stale title of issue #182.
#[test]
fn a_render_that_wrote_nothing_commits_nothing() {
    let fixture = TitleFixture::capable();
    let shown = shown_title("STABLE");

    let deduplicated = fixture.commit(title_update(Some("STABLE"), TITLE_PANE_PATH, Some(&shown)));
    assert!(!deduplicated.wrote());
    assert_eq!(
        deduplicated.committed(),
        None,
        "a fully deduplicated render must not overwrite the remembered title"
    );

    let changed = title_update(Some("CHANGED"), TITLE_PANE_PATH, Some(&shown));
    let wrote = fixture.commit(changed);
    assert_eq!(
        wrote.committed().and_then(ClientTitleState::title),
        Some("CHANGED"),
        "a render that wrote OSC 0 commits what the terminal now shows"
    );

    // A terminal that cannot write the sequence commits nothing either: it was
    // never told, so the next render must still consider the title pending.
    let incapable = TitleFixture::incapable().commit(changed);
    assert_eq!(incapable.committed(), None);
}

#[test]
fn attach_start_queries_client_theme_reports() {
    let terminal = xterm_with([]);

    let start = utf8(terminal.attach_start_sequence());
    let stop = utf8(terminal.attach_stop_sequence());

    assert!(start.contains("\u{1b}[?2031h"));
    assert!(start.contains("\u{1b}[?996n"));
    assert!(stop.contains("\u{1b}[?2031l"));
}

#[test]
fn cursor_style_transition_preserves_terminal_default_on_initial_default_attach() {
    let terminal = xterm_with([]);

    assert_eq!(terminal.render_cursor_style_transition(None, 0), None);
}

#[test]
fn cursor_style_transition_resets_only_when_leaving_an_explicit_style() {
    let terminal = xterm_with([]);

    assert_eq!(
        terminal.render_cursor_style_transition(Some(6), 0),
        Some("\u{1b}[2 q".to_owned())
    );
    assert_eq!(
        terminal.render_cursor_style_transition(Some(0), 6),
        Some("\u{1b}[6 q".to_owned())
    );
    assert_eq!(terminal.render_cursor_style_transition(Some(6), 6), None);
}

#[test]
fn clipboard_encoding_honours_feature_and_set_clipboard_option() {
    let enabled = xterm_with([(OptionName::SetClipboard, "external", SetOptionMode::Replace)]);
    let encoded = utf8(
        enabled
            .encode_forced_clipboard_set(b"hi")
            .expect("clipboard write is available"),
    );
    assert_eq!(encoded, "\u{1b}]52;;aGk=\u{7}");
    // Under `external` an application's inbound OSC 52 is NOT relayed to the
    // outer terminal: tmux gates that path on set-clipboard == on only
    // (input.c input_osc_52 returns early unless set-clipboard == 2), so an
    // untrusted pane cannot drive the system clipboard under the default.
    assert!(!enabled.clipboard_passthrough_enabled());
    // tmux's own selections (copy-mode yank / `set-buffer -w`) still forward
    // under `external` (window-copy.c gates them on set-clipboard != 0).
    assert!(enabled.encode_clipboard_set(b"hi").is_some());

    let on = xterm_with([(OptionName::SetClipboard, "on", SetOptionMode::Replace)]);
    // `on` is the opt-in that relays inbound application OSC 52 to the outer.
    assert!(on.clipboard_passthrough_enabled());

    let disabled = xterm_with([(OptionName::SetClipboard, "off", SetOptionMode::Replace)]);
    assert!(!disabled.clipboard_passthrough_enabled());
    assert!(disabled.encode_forced_clipboard_set(b"hi").is_some());
}

#[test]
fn sync_wrapper_brackets_render_frames_when_supported() {
    let terminal = xterm_with([(
        OptionName::TerminalFeatures,
        "xterm*:sync",
        SetOptionMode::Append,
    )]);

    let wrapped = utf8(terminal.wrap_render_frame(b"frame"));
    assert_eq!(wrapped, "\u{1b}[?2026hframe\u{1b}[?2026l");
}

#[test]
fn terminal_override_can_disable_sync_wrapper_after_feature_match() {
    let terminal = xterm_with([
        (
            OptionName::TerminalFeatures,
            "xterm*:sync",
            SetOptionMode::Append,
        ),
        (
            OptionName::TerminalOverrides,
            "xterm*:Sync@",
            SetOptionMode::Append,
        ),
    ]);

    assert_eq!(terminal.wrap_render_frame(b"frame"), b"frame");
    assert!(!terminal
        .features_string()
        .split(',')
        .any(|feature| feature == "sync"));
}

#[test]
fn decode_capability_string_handles_octal_escapes() {
    for (encoded, decoded, message) in [
        ("\\033[H", "\x1b[H", "\\033 should decode to ESC"),
        ("\\007", "\x07", "\\007 should decode to BEL"),
        ("\\0", "\x00", "\\0 alone should decode to NUL"),
    ] {
        assert_eq!(
            super::decode_capability_string(encoded),
            decoded,
            "{message}"
        );
    }
}

#[test]
fn decode_capability_string_handles_vis_escapes() {
    for (encoded, decoded, message) in [
        ("\\s", " ", "\\s should decode to space"),
        ("\\v", "\x0b", "\\v should decode to vertical tab"),
        ("\\^C", "\x03", "\\^C should decode to ctrl-C"),
        ("\\^?", "\x7f", "\\^? should decode to DEL"),
    ] {
        assert_eq!(
            super::decode_capability_string(encoded),
            decoded,
            "{message}"
        );
    }
}

#[test]
fn decode_capability_string_preserves_existing_escapes() {
    for (encoded, decoded) in [
        ("\\E[H", "\x1b[H"),
        ("\\e[H", "\x1b[H"),
        ("\\n", "\n"),
        ("\\\\", "\\"),
        ("\\:", ":"),
        ("\\", "\\"),
    ] {
        assert_eq!(super::decode_capability_string(encoded), decoded);
    }
}

#[test]
fn decode_capability_string_with_mixed_octal_and_text() {
    assert_eq!(
        super::decode_capability_string("\\033[?2026%p1%dq"),
        "\x1b[?2026%p1%dq"
    );
}

#[test]
fn override_with_octal_encoded_value_resolves_correctly() {
    let options = option_store([(
        ScopeSelector::Global,
        OptionName::TerminalOverrides,
        "dumb*:Ss=\\033[%p1%d q",
        SetOptionMode::Append,
    )]);
    let terminal = resolve_term(&options, "dumb");

    let style = terminal
        .render_cursor_style(2)
        .expect("cursor style should be available");
    assert_eq!(style, "\x1b[2 q");
}

#[test]
fn split_override_segments_handles_escaped_colons_and_empty_segments() {
    for (entry, expected) in [
        ("a::b:c", vec!["a:b", "c"]),
        ("pattern:", vec!["pattern", ""]),
        ("", vec![""]),
    ] {
        assert_eq!(super::split_override_segments(entry), expected);
    }
}

#[test]
fn empty_term_skips_feature_and_override_matching() {
    let options = OptionStore::new();
    let terminal = OuterTerminal::resolve(&options, OuterTerminalContext::default());
    assert_eq!(terminal.features_string(), "");
}

#[test]
fn sync_wrapper_passes_through_empty_frames() {
    let terminal = xterm_with([(
        OptionName::TerminalFeatures,
        "xterm*:sync",
        SetOptionMode::Append,
    )]);
    let wrapped = terminal.wrap_render_frame(b"");
    assert!(wrapped.is_empty());
}

#[test]
fn sanitize_osc_payload_strips_bel_and_esc() {
    let sanitized = super::sanitize_osc_payload("hello\x07world\x1b[0m");
    assert!(!sanitized.contains('\x07'));
    assert!(!sanitized.contains('\x1b'));
    assert_eq!(sanitized, "hello world [0m");
}

#[test]
fn base64_encoding_edge_cases() {
    for (input, expected) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(super::encode_base64(input.as_bytes()), expected);
    }
}

#[test]
fn clipboard_encoding_rejects_empty_bytes() {
    let terminal = xterm_with([(OptionName::SetClipboard, "on", SetOptionMode::Replace)]);
    assert!(terminal.encode_forced_clipboard_set(b"").is_none());
}

#[test]
fn colour_to_rgb_none_default_terminal_return_none() {
    assert!(super::colour_to_rgb(super::COLOUR_NONE).is_none());
    assert!(super::colour_to_rgb(super::COLOUR_DEFAULT).is_none());
    assert!(super::colour_to_rgb(super::COLOUR_TERMINAL).is_none());
}

#[test]
fn colour_to_rgb_256_palette_boundaries() {
    for (index, rgb) in [
        (0, (0, 0, 0)),         // basic black
        (15, (255, 255, 255)),  // basic bright white
        (16, (0, 0, 0)),        // first cube colour (black)
        (231, (255, 255, 255)), // last cube colour (white)
        (232, (8, 8, 8)),       // first greyscale
        (255, (238, 238, 238)), // last greyscale
    ] {
        assert_eq!(
            super::colour_to_rgb(super::COLOUR_FLAG_256 | index),
            Some(rgb),
            "index {index}"
        );
    }
}

#[test]
fn colour_to_rgb_bright_ansi_colours() {
    // SGR 90 = bright black
    assert_eq!(super::colour_to_rgb(90), Some((127, 127, 127)));
    // SGR 97 = bright white
    assert_eq!(super::colour_to_rgb(97), Some((255, 255, 255)));
}

#[test]
fn transition_sequence_emits_disable_then_enable_on_change() {
    let options = option_store([(
        ScopeSelector::Global,
        OptionName::FocusEvents,
        "on",
        SetOptionMode::Replace,
    )]);
    let with_focus = resolve_alpha(&options, xterm_context());
    let without_focus = resolve_alpha(&OptionStore::new(), xterm_context());

    // Transition from focus-enabled to focus-disabled should emit disable.
    let seq = utf8(without_focus.transition_sequence_from(&with_focus));
    assert!(seq.contains("\u{1b}[?1004l"));

    // Transition from focus-disabled to focus-enabled should emit enable.
    let seq = utf8(with_focus.transition_sequence_from(&without_focus));
    assert!(seq.contains("\u{1b}[?1004h"));
}

#[test]
fn transition_sequence_toggles_mouse_reporting_with_session_scope() {
    let options = option_store([(
        ScopeSelector::Global,
        OptionName::Mouse,
        "on",
        SetOptionMode::Replace,
    )]);
    let enabled = resolve_alpha(&options, xterm_context());
    let disabled = resolve_alpha(&OptionStore::new(), xterm_context());

    let seq = utf8(disabled.transition_sequence_from(&enabled));
    assert_contains_all(&seq, &MOUSE_DISABLE_PARTS);

    let seq = utf8(enabled.transition_sequence_from(&disabled));
    assert_contains_all(&seq, &MOUSE_ENABLE_PARTS);
}

#[test]
fn parse_capability_override_edge_cases() {
    for (spec, expected_name, expected_value, expected_remove) in [
        // Bare name (no = or @)
        ("Tc", "Tc", None, false),
        // Remove with @
        ("AX@", "AX", None, true),
        // Value with =
        ("Ss=\\E[q", "Ss", Some("\\E[q"), false),
        // Whitespace trimmed
        ("  Tc  ", "Tc", None, false),
    ] {
        let (name, value, remove) = super::parse_capability_override(spec).unwrap();
        assert_eq!(name, expected_name, "{spec:?}");
        assert_eq!(value, expected_value, "{spec:?}");
        assert_eq!(remove, expected_remove, "{spec:?}");
    }

    // Empty string
    assert!(super::parse_capability_override("").is_none());
}

#[test]
fn override_removal_wins_over_xt_reintroduction() {
    // XT triggers bpaste/focus/title, but if an explicit Enbp@ override
    // removes bpaste, the second override pass must honour the removal.
    let options = option_store([(
        ScopeSelector::Global,
        OptionName::TerminalOverrides,
        "custom*:XT:Enbp@:Dsbp@",
        SetOptionMode::Append,
    )]);
    let terminal = resolve_term(&options, "custom-term");

    let features = terminal.features_string();
    // XT should enable focus and title.
    assert!(features.contains("focus"), "focus should be active");
    assert!(features.contains("title"), "title should be active");
    // But bpaste was explicitly removed.
    assert!(
        !features.contains("bpaste"),
        "bpaste should be removed by override"
    );
}

#[test]
fn override_removal_wins_over_tc_rgb() {
    // Tc triggers RGB, but if AX@ removes default_colours, RGB should
    // still be set (Tc only controls RGB, not AX). Verify Tc works and
    // AX@ is independent.
    let options = option_store([(
        ScopeSelector::Global,
        OptionName::TerminalOverrides,
        "plain*:Tc:AX@",
        SetOptionMode::Append,
    )]);
    let terminal = resolve_term(&options, "plain-term");

    let features = terminal.features_string();
    assert!(features.contains("RGB"), "Tc should enable RGB");
}
