use super::*;

#[test]
fn collapse_or_parent_moves_to_parent_when_already_collapsed() {
    let build = tree_build(vec![
        tree_item("parent", None, &["child"], 0),
        tree_item("child", Some("parent"), &[], 1),
    ]);
    let mut mode = test_mode(10);
    mode.selected_id = Some("child".to_owned());
    collapse_or_parent(&mut mode, &build);
    assert_eq!(mode.selected_id.as_deref(), Some("parent"));
}

#[test]
fn expand_or_child_expands_then_enters() {
    let build = tree_build(vec![
        tree_item("parent", None, &["child"], 0),
        tree_item("child", Some("parent"), &[], 1),
    ]);
    let mut mode = test_mode(10);
    mode.selected_id = Some("parent".to_owned());
    // First right-arrow: expand
    expand_or_child(&mut mode, &build);
    assert!(mode.expanded.contains("parent"));
    assert_eq!(mode.selected_id.as_deref(), Some("parent"));
    // Second right-arrow: enter child
    expand_or_child(&mut mode, &build);
    assert_eq!(mode.selected_id.as_deref(), Some("child"));
}

#[test]
fn stable_order_reverses_primary_but_not_tiebreaker() {
    // When primary keys are equal, tiebreaker should NOT be reversed.
    let result = stable_order(Ordering::Equal, true, "a", "b");
    assert_eq!(result, Ordering::Less, "tiebreaker keeps natural order");

    // When primary keys differ, they should be reversed.
    let result = stable_order(Ordering::Less, true, "a", "b");
    assert_eq!(result, Ordering::Greater, "primary is reversed");
}

#[test]
fn expand_or_child_skips_ghost_children() {
    // "ghost" is listed as a child but not in items
    let build = tree_build(vec![tree_item("parent", None, &["ghost"], 0)]);
    let mut mode = test_mode(10);
    mode.selected_id = Some("parent".to_owned());
    mode.expanded.insert("parent".to_owned());
    // Expanding into ghost: selected should stay on parent (no valid child)
    expand_or_child(&mut mode, &build);
    assert_eq!(mode.selected_id.as_deref(), Some("parent"));
}

#[test]
fn move_selection_resets_preview_scroll() {
    let build = flat_build(&["a", "b", "c"]);
    let mut mode = test_mode(10);
    mode.selected_id = Some("a".to_owned());
    mode.preview_scroll = 5;
    move_selection(&mut mode, &build, 1, true);
    assert_eq!(mode.selected_id.as_deref(), Some("b"));
    assert_eq!(
        mode.preview_scroll, 0,
        "preview_scroll reset on selection change"
    );
}

#[test]
fn move_selection_preserves_preview_scroll_when_at_boundary() {
    let build = flat_build(&["a", "b", "c"]);
    let mut mode = test_mode(10);
    mode.selected_id = Some("c".to_owned());
    mode.preview_scroll = 3;
    // Page-style movement past the end should not change selection.
    move_selection(&mut mode, &build, 1, false);
    assert_eq!(mode.selected_id.as_deref(), Some("c"));
    assert_eq!(
        mode.preview_scroll, 3,
        "preview_scroll unchanged when selection doesn't change"
    );
}

#[test]
fn search_resets_preview_scroll_on_match() {
    let build = flat_build(&["alpha", "beta", "gamma"]);
    let mut mode = test_mode(10);
    mode.selected_id = Some("alpha".to_owned());
    mode.preview_scroll = 7;
    mode.search = Some(SearchState {
        value: "gamma".to_owned(),
        direction: SearchDirection::Forward,
    });
    repeat_search(&mut mode, &build, false);
    assert_eq!(mode.selected_id.as_deref(), Some("gamma"));
    assert_eq!(mode.preview_scroll, 0);
}

#[test]
fn parse_choose_tree_with_template_and_flags() {
    let mode = parse_mode_tree_source("choose-tree -sZ display-message")
        .expect("ok")
        .expect("recognized");
    assert_eq!(mode.tree_depth, TreeDepth::Session);
    assert!(mode.zoom);
    assert_eq!(mode.template.as_deref(), Some("display-message"));
}

#[test]
fn parse_choose_tree_preserves_single_argv_template_for_runtime_reparse() {
    for template in [
        "switch-client -t target",
        "switch-client -t '%%'",
        "switch-client -Zt '%%'",
        "switch-client -t '#{?#{==:#{session_name},target},%%,missing}'",
        "switch-client -t '%%' ; display-message -p selected",
    ] {
        let mode = parse_mode_tree(&["choose-tree", template]);
        assert_eq!(mode.template.as_deref(), Some(template));
    }
}

#[test]
fn parse_choose_tree_preserves_quoted_source_template_for_runtime_reparse() {
    let template = "switch-client -t '%%' ; display-message -p selected";
    let mode = parse_mode_tree_source(&format!("choose-tree {template:?}"))
        .expect("ok")
        .expect("recognized");
    assert_eq!(mode.template.as_deref(), Some(template));
}

#[test]
fn parse_choose_tree_zw_preserves_trailing_direct_command_arguments_from_argv() {
    let mode = parse_mode_tree(&["choose-tree", "-Zw", "set-buffer", "-b", "chosen", "%%"]);
    assert_eq!(mode.tree_depth, TreeDepth::Window);
    assert!(mode.zoom);
    assert_eq!(
        mode.template.as_deref(),
        Some("set-buffer -b chosen \"%%\"")
    );
}

#[test]
fn parse_choose_tree_double_dash_separates_template() {
    let mode = parse_mode_tree_source("choose-tree -- -not-a-flag")
        .expect("ok")
        .expect("recognized");
    assert_eq!(mode.template.as_deref(), Some("-not-a-flag"));
}

#[test]
fn customize_mode_rejects_extra_argument() {
    assert!(parse_mode_tree_source("customize-mode unexpected").is_err());
}
