use super::*;
use rmux_proto::SplitDirection::{Horizontal, Vertical};

/// Invalid legacy `-p` spellings that tmux ignores once `-l` sets the size.
const IGNORED_LEGACY_PERCENTAGES: [&[&str]; 4] =
    [&["-pabc"], &["-p", "abc"], &["-p", "-1"], &["-p", "256"]];

/// `join-pane` source and destination shared by the size cases.
const JOIN_PANES: &[&str] = &["join-pane", "-s", "alpha:0.1", "-t", "alpha:0.0"];

/// `move-pane` source and destination shared by the size cases.
const MOVE_PANES: &[&str] = &["move-pane", "-s", "alpha:0.1", "-t", "beta:1.2"];

/// Source and destination shared by the `join-pane` and `move-pane` direction cases.
const JOIN_SOURCE_AND_TARGET: &[&str] = &["-s", "alpha:0.1", "-t", "alpha:1.0"];

/// Parses a `split-window`, `join-pane` or `move-pane` command line and returns its size request.
fn size_spec(argv: &[&str]) -> Option<String> {
    match (
        argv[0],
        parse_args(argv).expect("command line parses").command,
    ) {
        ("split-window", Some(Command::SplitWindow(args))) => args.size_spec(),
        ("join-pane", Some(Command::JoinPane(args)))
        | ("move-pane", Some(Command::MovePane(args))) => args.size_spec(),
        (name, other) => panic!("expected {name} command, got {other:?}"),
    }
}

#[test]
fn split_window_direction_defaults_to_vertical_and_prefers_horizontal_like_tmux() {
    for (argv, direction) in [
        (&["split-window", "-t", "alpha"][..], Vertical),
        (&["split-window", "-h", "-t", "alpha"][..], Horizontal),
    ] {
        let args = parse_command!(SplitWindow, argv);
        assert_eq!(args.horizontal, direction == Horizontal, "{argv:?}");
        assert!(!args.vertical, "{argv:?}");
        assert_eq!(args.direction(), direction, "{argv:?}");
    }
    for argv in [
        ["split-window", "-h", "-v", "-t", "alpha"],
        ["split-window", "-v", "-h", "-t", "alpha"],
    ] {
        let args = parse_command!(SplitWindow, argv);
        assert_eq!(args.direction(), Horizontal, "{argv:?}");
    }
}

#[test]
fn list_panes_accepts_session_target_and_optional_format() {
    let args = parse_command!(ListPanes, ["list-panes", "-t", "alpha", "-F", "#{pane_id}"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha");
    assert_eq!(args.format.as_deref(), Some("#{pane_id}"));
    assert!(args.filter.is_none());
    assert!(!args.all_sessions);
    assert!(!args.session_scope);
}

#[test]
fn list_panes_accepts_tmux_filter_flag() {
    let args = parse_command!(
        ListPanes,
        [
            "list-panes",
            "-t",
            "$1",
            "-f",
            "#{m:%0,#{pane_id}}",
            "-F",
            "#{pane_id}",
        ]
    );
    assert_eq!(target_text(args.target.as_ref()), "$1");
    assert_eq!(args.filter.as_deref(), Some("#{m:%0,#{pane_id}}"));
    assert_eq!(args.format.as_deref(), Some("#{pane_id}"));
}

#[test]
fn list_panes_accepts_all_sessions_and_session_scope_without_a_target() {
    let args = parse_command!(ListPanes, ["list-panes", "-a", "-s"]);
    assert!(args.all_sessions);
    assert!(args.session_scope);
    assert!(args.target.is_none());
    assert!(args.format.is_none());
    assert!(args.filter.is_none());
}

#[test]
fn split_window_attached_cluster_values_preserve_target_and_size() {
    let args = parse_command!(SplitWindow, ["split-window", "-htalpha:0", "sleep", "1"]);
    assert_eq!(args.direction(), Horizontal);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0");
    assert_eq!(args.command, ["sleep", "1"]);

    for (argv, size) in [
        (["split-window", "-hl10", "-t", "alpha"], "10"),
        (["split-window", "-hp50", "-t", "alpha"], "50%"),
    ] {
        let args = parse_command!(SplitWindow, argv);
        assert_eq!(args.direction(), Horizontal, "{argv:?}");
        assert_eq!(args.size_spec().as_deref(), Some(size), "{argv:?}");
    }
}

#[test]
fn split_join_and_move_pane_percentages_match_tmux_and_explicit_lengths_take_priority() {
    for argv in [
        vec!["split-window", "-p", "50", "-t", "alpha"],
        [JOIN_PANES, &["-p", "50"]].concat(),
        vec![
            "join-pane",
            "-p",
            "50",
            "-s",
            "alpha:0.1",
            "-t",
            "alpha:0.0",
        ],
    ] {
        assert_eq!(size_spec(&argv).as_deref(), Some("50%"), "{argv:?}");
    }
    let move_percent = ["move-pane", "-p", "35", "-s", "alpha:0.1", "-t", "beta:1.2"];
    assert_eq!(size_spec(&move_percent).as_deref(), Some("35%"));

    for argv in [
        vec!["split-window", "-l", "5", "-p", "50", "-t", "alpha"],
        vec!["split-window", "-p", "50", "-l", "5", "-t", "alpha"],
        [JOIN_PANES, &["-l", "5", "-p", "50"]].concat(),
        [JOIN_PANES, &["-p", "50", "-l", "5"]].concat(),
    ] {
        assert_eq!(size_spec(&argv).as_deref(), Some("5"), "{argv:?}");
    }
    for legacy in IGNORED_LEGACY_PERCENTAGES {
        for argv in [
            [&["split-window"][..], legacy, &["-l", "5", "-t", "alpha"]].concat(),
            [JOIN_PANES, legacy, &["-l", "5"]].concat(),
            [MOVE_PANES, legacy, &["-l", "5"]].concat(),
        ] {
            assert_eq!(size_spec(&argv).as_deref(), Some("5"), "{argv:?}");
        }
    }
}

#[test]
fn resize_pane_direction_clusters_and_trailing_adjustment_follow_tmux() {
    for (argv, left) in [
        (&["resize-pane", "-RL", "-t", "alpha:0.0"][..], 1),
        (&["resize-pane", "-R", "-L", "-t", "alpha:0.0", "3"][..], 3),
    ] {
        let args = parse_command!(ResizePane, argv);
        assert_eq!(args.left, Some(left), "{argv:?}");
        assert!(args.right.is_none(), "{argv:?}");
    }
}

#[test]
fn resize_pane_accepts_zoom_alone_and_after_columns_like_tmux() {
    for (argv, columns) in [
        (&["resize-pane", "-t", "alpha:0.1", "-Z"][..], None),
        (
            &["resize-pane", "-t", "alpha:0.1", "-x", "34", "-Z"][..],
            Some(super::super::ResizePaneSize::Cells(34)),
        ),
    ] {
        let args = parse_command!(ResizePane, argv);
        assert_eq!(target_text(args.target.as_ref()), "alpha:0.1", "{argv:?}");
        assert!(args.zoom, "{argv:?}");
        assert_eq!(args.columns, columns, "{argv:?}");
    }
}

#[test]
fn display_panes_accepts_published_client_names() {
    let args = parse_command!(DisplayPanes, ["display-panes", "-t", "/dev/pts/7"]);
    assert_eq!(args.target_client.as_deref(), Some("/dev/pts/7"));
}

#[test]
fn split_window_accepts_trailing_command_argv() {
    let args = parse_command!(
        SplitWindow,
        [
            "split-window",
            "-h",
            "-t",
            "alpha",
            "sh",
            "-c",
            "printf split-command",
        ]
    );
    assert!(args.horizontal);
    assert_eq!(args.command, ["sh", "-c", "printf split-command"]);
}

#[test]
fn split_window_preserves_glued_flags_after_trailing_command_starts() {
    let args = parse_command!(
        SplitWindow,
        [
            "split-window",
            "-t",
            "alpha",
            "bash",
            "-lc",
            "printf split-command",
        ]
    );
    assert_eq!(args.command, ["bash", "-lc", "printf split-command"]);
}

#[test]
fn split_window_allows_trailing_command_l_flag() {
    for argv in [
        &["split-window", "-d", "ls", "-l"][..],
        &["split-window", "-d", "--", "ls", "-l"],
    ] {
        let args = parse_command!(SplitWindow, argv);
        assert!(args.detached, "{argv:?}");
        assert_eq!(args.command, ["ls", "-l"], "{argv:?}");
    }
}

#[test]
fn split_window_accepts_start_directory_before_trailing_command() {
    let args = parse_command!(
        SplitWindow,
        [
            "split-window",
            "-h",
            "-c",
            "/tmp/work",
            "-t",
            "alpha",
            "sh",
            "-c",
            "pwd",
        ]
    );
    assert!(args.horizontal);
    assert_eq!(args.start_directory, Some(PathBuf::from("/tmp/work")));
    assert_eq!(args.command, ["sh", "-c", "pwd"]);
}

#[test]
fn split_window_accepts_tmux_compat_flags_before_command() {
    let args = parse_command!(
        SplitWindow,
        [
            "split-window",
            "-b",
            "-d",
            "-Z",
            "-l",
            "12",
            "-t",
            "alpha:0.0",
            "sh",
        ]
    );
    assert!(args.before);
    assert!(args.detached);
    assert!(args.preserve_zoom);
    assert_eq!(args.size.as_deref(), Some("12"));
    assert_eq!(args.command, ["sh"]);
}

#[test]
fn split_window_accepts_percentage_size_like_tmux() {
    let args = parse_command!(
        SplitWindow,
        ["split-window", "-p25", "-f", "-t", "alpha:0.0"]
    );
    assert!(args.full_size);
    assert_eq!(args.size_spec().as_deref(), Some("25%"));
}

#[test]
fn split_window_accepts_full_size_flag() {
    let args = parse_command!(SplitWindow, ["split-window", "-f", "-t", "alpha:0.0"]);
    assert_eq!(args.size_spec(), None);
    assert!(args.full_size);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0.0");
}

#[test]
fn split_window_rejects_unknown_flag_before_trailing_command() {
    let error = parse_error(&["split-window", "-Q", "printf ok"]);
    assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    assert!(
        error
            .to_string()
            .contains("command split-window: unknown flag -Q")
    );
}

#[test]
fn split_window_preserves_hyphenated_values_after_trailing_command_starts() {
    let args = parse_command!(SplitWindow, ["split-window", "env", "-Q", "value"]);
    assert_eq!(args.command, ["env", "-Q", "value"]);
}

#[test]
fn split_window_parses_stdin_flag_without_treating_it_as_command() {
    let args = parse_command!(SplitWindow, ["split-window", "-I", "-t", "alpha:0.0"]);
    assert!(args.stdin);
    assert!(args.command.is_empty());
}

#[test]
fn split_window_parses_compact_keep_alive_flag() {
    let args = parse_command!(
        SplitWindow,
        ["split-window", "-dk", "-t", "alpha:0.0", "exit 7"]
    );
    assert!(args.detached);
    assert!(args.keep_alive_on_exit);
    assert_eq!(args.command, ["exit 7"]);
}

#[test]
fn swap_pane_relative_flags_need_no_source_and_follow_tmux_priority() {
    for argv in [
        &["swap-pane", "-D", "-t", "alpha:2.3"][..],
        &["swap-pane", "-D", "-U", "-t", "alpha:2.3"],
        &["swap-pane", "-U", "-D", "-t", "alpha:2.3"],
    ] {
        let args = parse_command!(SwapPane, argv);
        assert!(args.down, "{argv:?}");
        assert!(!args.up, "{argv:?}");
        assert!(args.source.is_none(), "{argv:?}");
        assert!(args.uses_relative_target(), "{argv:?}");
        assert_eq!(target_text(args.target.as_ref()), "alpha:2.3", "{argv:?}");
    }
}

#[test]
fn swap_pane_accepts_explicit_source_and_target_panes() {
    let args = parse_command!(
        SwapPane,
        ["swap-pane", "-s", "alpha:0.1", "-t", "beta:3.2", "-d"]
    );
    assert!(args.detached);
    assert_eq!(target_text(args.source.as_ref()), "alpha:0.1");
    assert_eq!(target_text(args.target.as_ref()), "beta:3.2");
    assert!(!args.uses_relative_target());
}

#[test]
fn swap_pane_accepts_zoom_preservation_flag() {
    let args = parse_command!(
        SwapPane,
        ["swap-pane", "-Z", "-s", "alpha:0.1", "-t", "beta:3.2"]
    );
    assert!(args.preserve_zoom);
    assert_eq!(target_text(args.source.as_ref()), "alpha:0.1");
    assert_eq!(target_text(args.target.as_ref()), "beta:3.2");
}

#[test]
fn join_pane_defaults_to_vertical_direction() {
    let args = parse_command!(
        JoinPane,
        ["join-pane", "-s", "alpha:0.1", "-t", "alpha:1.0"]
    );
    assert_eq!(target_text(args.source.as_ref()), "alpha:0.1");
    assert_eq!(target_text(args.target.as_ref()), "alpha:1.0");
    assert_eq!(args.direction(), Vertical);
}

#[test]
fn join_and_move_pane_direction_flags_follow_tmux_priority() {
    for flags in [&["-h", "-v"][..], &["-v", "-h"], &["-hv"], &["-vh"]] {
        let join = [&["join-pane"][..], flags, JOIN_SOURCE_AND_TARGET].concat();
        let args = parse_command!(JoinPane, join);
        assert_eq!(args.direction(), Horizontal, "{join:?}");
        let move_pane = [&["move-pane"][..], flags, JOIN_SOURCE_AND_TARGET].concat();
        let args = parse_command!(MovePane, move_pane);
        assert_eq!(args.direction(), Horizontal, "{move_pane:?}");
    }
}

#[test]
fn join_pane_accepts_implicit_marked_source() {
    let args = parse_command!(JoinPane, ["join-pane", "-t", "alpha:1.0"]);
    assert!(args.source.is_none());
    assert_eq!(target_text(args.target.as_ref()), "alpha:1.0");
}

#[test]
fn join_pane_accepts_before_full_size_and_percentage_length_flags() {
    let args = parse_command!(
        JoinPane,
        [
            "join-pane",
            "-b",
            "-f",
            "-l",
            "30%",
            "-s",
            "alpha:0.1",
            "-t",
            "alpha:1.0",
        ]
    );
    assert!(args.before);
    assert!(args.full_size);
    assert_eq!(args.size_spec().as_deref(), Some("30%"));
}

#[test]
fn move_pane_parses_the_full_join_pane_flag_surface() {
    let args = parse_command!(
        MovePane,
        [
            "move-pane",
            "-b",
            "-d",
            "-f",
            "-h",
            "-l",
            "12",
            "-s",
            "alpha:0.1",
            "-t",
            "beta:1.2",
        ]
    );
    assert!(args.before);
    assert!(args.detached);
    assert!(args.full_size);
    assert_eq!(args.direction(), Horizontal);
    assert_eq!(args.size_spec().as_deref(), Some("12"));
    assert_eq!(target_text(args.source.as_ref()), "alpha:0.1");
    assert_eq!(target_text(args.target.as_ref()), "beta:1.2");
}

#[test]
fn break_pane_accepts_optional_target_and_name() {
    let args = parse_command!(
        BreakPane,
        [
            "break-pane",
            "-s",
            "alpha:1.2",
            "-t",
            "beta:4",
            "-n",
            "logs",
        ]
    );
    assert_eq!(target_text(args.source.as_ref()), "alpha:1.2");
    assert_eq!(target_text(args.target.as_ref()), "beta:4");
    assert_eq!(args.name.as_deref(), Some("logs"));
}

#[test]
fn break_pane_accepts_placement_and_print_flags() {
    let args = parse_command!(
        BreakPane,
        [
            "break-pane",
            "-a",
            "-P",
            "-F",
            "#{window_index}.#{pane_index}",
            "-s",
            "alpha:1.2",
        ]
    );
    assert!(args.after);
    assert!(!args.before);
    assert!(args.print_target);
    assert_eq!(
        args.format.as_deref(),
        Some("#{window_index}.#{pane_index}")
    );
}

#[test]
fn select_layout_accepts_all_standard_layout_names() {
    for layout in [
        "main-vertical",
        "main-horizontal",
        "even-horizontal",
        "even-vertical",
        "tiled",
    ] {
        let args = parse_command!(SelectLayout, ["select-layout", "-t", "alpha:0", layout]);
        assert_eq!(args.layout.as_deref(), Some(layout));
    }
}

#[test]
fn select_layout_accepts_old_layout_flag() {
    let args = parse_command!(SelectLayout, ["select-layout", "-o", "-t", "alpha:0"]);
    assert!(args.old);
    assert_eq!(target_text(args.target.as_ref()), "alpha:0");
    assert!(args.layout.is_none());
}

#[test]
fn select_layout_accepts_tmux_mode_clusters_and_preserves_all_flags() {
    // Each spelling parses like tmux 3.7b.
    for argv in [
        &["select-layout", "-En", "-t", "alpha:0"][..],
        &["select-layout", "-nE", "-t", "alpha:0"],
        &["select-layout", "-E", "-n", "-t", "alpha:0"],
        &["select-layout", "-Enop", "-t", "alpha:0", "tiled"],
    ] {
        let args = parse_command!(SelectLayout, argv);
        assert!(args.next, "-n must survive parsing for {argv:?}");
        assert_eq!(target_text(args.target.as_ref()), "alpha:0", "{argv:?}");
        if argv.contains(&"-Enop") {
            assert!(args.spread);
            assert!(args.old);
            assert!(args.previous);
            assert_eq!(args.layout.as_deref(), Some("tiled"));
        }
    }
}

#[test]
fn select_layout_old_mode_defers_its_optional_operand_to_runtime() {
    // tmux accepts one operand with -o and validates it as a custom layout.
    let args = parse_command!(SelectLayout, ["select-layout", "-o", "tiled"]);
    assert!(args.old);
    assert_eq!(args.layout.as_deref(), Some("tiled"));
}

#[test]
fn next_layout_accepts_window_targets() {
    let args = parse_command!(NextLayout, ["next-layout", "-t", "alpha:3"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha:3");
}

#[test]
fn previous_layout_preserves_session_targets_for_runtime_resolution() {
    let args = parse_command!(PreviousLayout, ["previous-layout", "-t", "alpha"]);
    assert_eq!(target_text(args.target.as_ref()), "alpha");
}
