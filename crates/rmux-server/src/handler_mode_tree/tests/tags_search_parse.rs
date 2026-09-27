use super::*;

#[test]
fn tree_item_keys_bind_to_stable_identities() {
    use super::super::mode_tree_order::{pane_item_id, session_item_id, window_item_id};

    let old_session = rmux_proto::SessionId::new(4);
    let replacement_session = rmux_proto::SessionId::new(5);
    let old_window = rmux_proto::WindowId::new(7);
    let replacement_window = rmux_proto::WindowId::new(8);
    let old_occurrence = crate::pane_terminals::WindowLinkOccurrenceId::new_for_test(13);
    let replacement_occurrence = crate::pane_terminals::WindowLinkOccurrenceId::new_for_test(14);
    let pane = rmux_proto::PaneId::new(11);

    assert_ne!(
        session_item_id(old_session),
        session_item_id(replacement_session)
    );
    assert_ne!(
        window_item_id(old_session, 3, old_window, old_occurrence),
        window_item_id(old_session, 3, replacement_window, old_occurrence)
    );
    assert_ne!(
        window_item_id(old_session, 3, old_window, old_occurrence),
        window_item_id(old_session, 3, old_window, replacement_occurrence)
    );
    assert_ne!(
        pane_item_id(old_session, 3, old_window, old_occurrence, pane),
        pane_item_id(replacement_session, 3, old_window, old_occurrence, pane,)
    );
}

#[test]
fn toggle_tag_does_not_tag_no_tag_items() {
    let build = tree_build(vec![
        ModeTreeItem {
            no_tag: true,
            ..tree_item("header", None, &["child"], 0)
        },
        tree_item("child", Some("header"), &[], 1),
    ]);
    let mut mode = test_mode(10);
    mode.selected_id = Some("header".to_owned());
    toggle_tag(&mut mode, &build);
    assert!(mode.tagged.is_empty());
}

#[test]
fn toggle_tag_untags_ancestors_and_descendants() {
    let build = tree_build(vec![
        tree_item("parent", None, &["child"], 0),
        tree_item("child", Some("parent"), &["grandchild"], 1),
        tree_item("grandchild", Some("child"), &[], 2),
    ]);
    let mut mode = test_mode(10);
    mode.tagged.insert("parent".to_owned());
    mode.tagged.insert("grandchild".to_owned());
    mode.selected_id = Some("child".to_owned());
    toggle_tag(&mut mode, &build);
    assert!(mode.tagged.contains("child"));
    assert!(
        !mode.tagged.contains("parent"),
        "ancestor should be untagged"
    );
    assert!(
        !mode.tagged.contains("grandchild"),
        "descendant should be untagged"
    );
}

#[test]
fn search_smart_case_lowercase_query_is_case_insensitive() {
    let build = flat_build(&["Alpha", "beta", "GAMMA"]);
    let mut mode = test_mode(10);
    mode.selected_id = Some("Alpha".to_owned());
    mode.search = Some(SearchState {
        value: "gamma".to_owned(),
        direction: SearchDirection::Forward,
    });
    repeat_search(&mut mode, &build, false);
    assert_eq!(mode.selected_id.as_deref(), Some("GAMMA"));
}

#[test]
fn search_mixed_case_query_is_case_sensitive() {
    let build = flat_build(&["Alpha", "beta", "GAMMA"]);
    let mut mode = test_mode(10);
    mode.selected_id = Some("Alpha".to_owned());
    mode.search = Some(SearchState {
        value: "Beta".to_owned(),
        direction: SearchDirection::Forward,
    });
    // "Beta" != "beta" in case-sensitive mode; no match
    repeat_search(&mut mode, &build, false);
    assert_eq!(mode.selected_id.as_deref(), Some("Alpha"));
}

#[test]
fn search_wraps_around() {
    let build = flat_build(&["a", "b", "c"]);
    let mut mode = test_mode(10);
    mode.selected_id = Some("c".to_owned());
    mode.search = Some(SearchState {
        value: "a".to_owned(),
        direction: SearchDirection::Forward,
    });
    repeat_search(&mut mode, &build, false);
    assert_eq!(mode.selected_id.as_deref(), Some("a"));
}

#[test]
fn search_backward_wraps_around() {
    let build = flat_build(&["a", "b", "c"]);
    let mut mode = test_mode(10);
    mode.selected_id = Some("a".to_owned());
    mode.search = Some(SearchState {
        value: "c".to_owned(),
        direction: SearchDirection::Backward,
    });
    repeat_search(&mut mode, &build, false);
    assert_eq!(mode.selected_id.as_deref(), Some("c"));
}

#[test]
fn parse_choose_tree_with_s_flag() {
    let mode = parse_mode_tree_source("choose-tree -s")
        .expect("ok")
        .expect("recognized");
    assert_eq!(mode.tree_depth, TreeDepth::Session);
}

#[test]
fn parse_choose_tree_with_w_flag() {
    let mode = parse_mode_tree_source("choose-tree -w")
        .expect("ok")
        .expect("recognized");
    assert_eq!(mode.tree_depth, TreeDepth::Window);
}

#[test]
fn parse_choose_tree_rejects_invalid_sort_order() {
    assert!(parse_mode_tree_source("choose-tree -O size").is_err());
}

#[test]
fn parse_choose_tree_rejects_unknown_flag() {
    assert!(parse_mode_tree_source("choose-tree -Q").is_err());
}

#[test]
fn parse_customize_mode_ignores_template_argument() {
    let mode = parse_mode_tree_source("customize-mode")
        .expect("ok")
        .expect("recognized");
    assert!(mode.template.is_none());
    assert_eq!(mode.kind, ModeTreeKind::Customize);
}

#[test]
fn n_flag_single_disables_preview() {
    let mode = parse_mode_tree_source("choose-buffer -N")
        .expect("ok")
        .expect("recognized");
    assert_eq!(mode.preview_mode, PreviewMode::Off);
}

#[test]
fn n_flag_double_enables_big_preview() {
    let mode = parse_mode_tree_source("choose-buffer -NN")
        .expect("ok")
        .expect("recognized");
    assert_eq!(mode.preview_mode, PreviewMode::Big);
}

#[test]
fn selected_items_returns_tagged_or_selected() {
    let build = flat_build(&["a", "b", "c"]);
    let mut mode = test_mode(10);
    mode.selected_id = Some("a".to_owned());
    // No tags: falls back to selected
    let items = selected_items(&mode, &build);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, "a");

    // With tags: returns tagged
    mode.tagged.insert("b".to_owned());
    mode.tagged.insert("c".to_owned());
    let items = selected_items(&mode, &build);
    assert_eq!(items.len(), 2);

    // Stale tags are a no-op; they must not fall back to an unrelated selection.
    mode.tagged.clear();
    mode.tagged.insert("missing".to_owned());
    assert!(selected_items(&mode, &build).is_empty());
}

#[test]
fn current_and_tagged_tree_kill_prompts_match_tmux_text() {
    let mut mode = test_mode(10);
    mode.selected_id = Some("window".to_owned());
    mode.tagged.insert("pane".to_owned());
    mode.tagged.insert("session".to_owned());

    let alpha = || SessionName::new("alpha").expect("valid session");
    let build = tree_build(vec![
        ModeTreeItem {
            action: ModeTreeAction::session_tree_target(alpha(), rmux_proto::SessionId::new(1)),
            ..tree_item("session", None, &["window"], 0)
        },
        ModeTreeItem {
            action: ModeTreeAction::window_tree_target(
                alpha(),
                rmux_proto::SessionId::new(1),
                3,
                rmux_proto::WindowId::new(7),
                crate::pane_terminals::WindowLinkOccurrenceId::new_for_test(13),
            ),
            ..tree_item("window", Some("session"), &["pane"], 1)
        },
        ModeTreeItem {
            action: ModeTreeAction::pane_tree_target(
                rmux_proto::PaneTarget::with_window(alpha(), 3, 1),
                rmux_proto::SessionId::new(1),
                rmux_proto::WindowId::new(7),
                crate::pane_terminals::WindowLinkOccurrenceId::new_for_test(13),
                rmux_proto::PaneId::new(42),
                7,
            ),
            ..tree_item("pane", Some("window"), &[], 2)
        },
    ]);

    assert_eq!(
        current_tree_kill_prompt(&mode, &build).as_deref(),
        Some("Kill window 3? ")
    );
    assert_eq!(
        tagged_tree_kill_prompt(&mode).as_deref(),
        Some("Kill 2 tagged? ")
    );
}
