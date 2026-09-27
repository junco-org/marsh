use super::*;

#[test]
fn parse_choose_buffer_command_preserves_preview_and_sort_flags() {
    let mode = parse_mode_tree_source("choose-buffer -NN -O size")
        .expect("mode-tree command parses")
        .expect("mode-tree command recognized");

    assert!(matches!(mode.kind, ModeTreeKind::Buffer));
    assert!(matches!(mode.preview_mode, PreviewMode::Big));
    assert_eq!(mode.sort_order, Some(SortOrder::Size));
}

#[test]
fn tag_all_descends_through_no_tag_headers() {
    let mut mode = ModeTreeClientState {
        kind: ModeTreeKind::Customize,
        session_name: SessionName::new("alpha").expect("valid session"),
        preview_mode: PreviewMode::Normal,
        order_seq: Vec::new(),
        ..test_mode(20)
    };
    let build = tree_build(vec![
        ModeTreeItem {
            no_tag: true,
            ..tree_item("root", None, &["header"], 0)
        },
        ModeTreeItem {
            no_tag: true,
            ..tree_item("header", Some("root"), &["leaf"], 1)
        },
        tree_item("leaf", Some("header"), &[], 2),
    ]);

    tag_all(&mut mode, &build);

    assert!(mode.tagged.contains("leaf"));
    assert!(!mode.tagged.contains("root"));
    assert!(!mode.tagged.contains("header"));
}
