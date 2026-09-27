use super::super::scripting_support::QueueExecutionContext;
use super::super::{DetachedRequesterAuthority, RequesterOrigin};
use super::mode_tree_build::tree_item_display_line;
use super::mode_tree_model::{
    ModeTreeAction, ModeTreeBuild, ModeTreeItem, PreviewMode, SearchDirection, SearchState,
    SortOrder, TreeDepth,
};
use super::mode_tree_preview::{
    preview_horizontal_offset, preview_lines_for_screen, preview_vertical_offset,
    render_preview_segment_row, render_tree_columns_preview, tree_preview_layout,
    tree_window_preview_label, PreviewColumn,
};
use super::mode_tree_render::{mode_tree_list_rows, render_mode_tree_overlay, render_visible_item};
use super::mode_tree_selection::{
    clamp_scroll, collapse_or_parent, current_tree_kill_prompt, cycle_sort,
    ensure_selected_visible, expand_or_child, move_selection, repeat_search, selected_items,
    tag_all, tagged_tree_kill_prompt, toggle_tag,
};
use super::mode_tree_sort::stable_order;
use super::*;
use crate::handler::test_support::{open_mode_tree, parse_mode_tree};
use crate::pane_io::AttachControl;
use crate::pane_terminals::HandlerState;
use crate::test_fixtures::Fixture;
use rmux_core::{command_parser::CommandParser, input::InputParser, Screen, Style, Utf8Config};
use rmux_proto::{
    OptionName, PaneTarget, Response, ScopeSelector, SessionName, SetOptionMode, SplitDirection,
    SplitWindowRequest, TerminalSize,
};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use tokio::sync::mpsc;

fn test_origin() -> RequesterOrigin {
    RequesterOrigin::new(std::process::id(), DetachedRequesterAuthority::Denied)
}

fn test_mode(list_rows: usize) -> ModeTreeClientState {
    ModeTreeClientState {
        origin: test_origin(),
        kind: ModeTreeKind::Tree,
        session_name: SessionName::new("test").expect("valid session"),
        session_id: rmux_proto::SessionId::new(1),
        host_pane: None,
        host_identity: None,
        host_transcript: None,
        preview_mode: PreviewMode::Off,
        row_format: None,
        filter_format: None,
        filter_text: None,
        key_format: DEFAULT_KEY_FORMAT.to_owned(),
        template: None,
        search: None,
        tagged: BTreeSet::new(),
        expanded: BTreeSet::new(),
        selected_id: None,
        scroll: 0,
        preview_scroll: 0,
        sort_order: None,
        order_seq: vec![SortOrder::Index, SortOrder::Name, SortOrder::Activity],
        reversed: false,
        tree_depth: TreeDepth::Pane,
        show_all_group_members: false,
        auto_accept: false,
        zoom_restore: None,
        last_list_rows: list_rows,
    }
}

/// A plain, taggable, unlabelled item `id` under `parent`, at `depth`.
fn tree_item(id: &str, parent: Option<&str>, children: &[&str], depth: usize) -> ModeTreeItem {
    ModeTreeItem {
        id: id.to_owned(),
        parent: parent.map(str::to_owned),
        children: children.iter().map(|child| (*child).to_owned()).collect(),
        depth,
        line: String::new(),
        search_text: String::new(),
        preview: Vec::new(),
        no_tag: false,
        action: ModeTreeAction::None,
    }
}

/// A build of `items` in display order, every item visible and the parentless ones roots.
fn tree_build(items: Vec<ModeTreeItem>) -> ModeTreeBuild {
    let order: Vec<String> = items.iter().map(|item| item.id.clone()).collect();
    let roots = items
        .iter()
        .filter(|item| item.parent.is_none())
        .map(|item| item.id.clone())
        .collect();
    ModeTreeBuild {
        items: items
            .into_iter()
            .map(|item| (item.id.clone(), item))
            .collect(),
        roots,
        visible: order.clone(),
        order,
        no_matches: false,
    }
}

fn flat_build(ids: &[&str]) -> ModeTreeBuild {
    tree_build(
        ids.iter()
            .map(|id| ModeTreeItem {
                line: (*id).to_owned(),
                search_text: (*id).to_owned(),
                ..tree_item(id, None, &[], 0)
            })
            .collect(),
    )
}

/// Parses the command line `source` as a mode-tree command.
fn parse_mode_tree_source(source: &str) -> Result<Option<ParsedModeTreeCommand>, RmuxError> {
    let parsed = CommandParser::new()
        .parse_one_group(source)
        .expect("parses");
    RequestHandler::parse_mode_tree_queue_command(parsed.commands()[0].clone())
}

/// Whether `frame` moves the cursor to the start of the one-based `row`.
fn frame_visits_row(frame: &[u8], row: u16) -> bool {
    let cursor = format!("\x1b[{row};1H");
    frame
        .windows(cursor.len())
        .any(|window| window == cursor.as_bytes())
}

/// Runs `edit` on `attach_pid`'s active mode tree while holding the attach lock.
async fn with_mode_tree<T>(
    handler: &RequestHandler,
    attach_pid: u32,
    edit: impl FnOnce(&mut ModeTreeClientState) -> T,
) -> T {
    let mut active_attach = handler.active_attach.lock().await;
    let mode = active_attach
        .by_pid
        .get_mut(&attach_pid)
        .and_then(|active| active.mode_tree.as_mut())
        .expect("mode tree remains active");
    edit(mode)
}

/// Confirms the open prompt with `y`, then waits until the confirmed action refreshes the
/// mode-tree overlay.
async fn confirm_prompt_and_wait_for_action(
    handler: &RequestHandler,
    attach_pid: u32,
    control_rx: &mut mpsc::UnboundedReceiver<AttachControl>,
) {
    while control_rx.try_recv().is_ok() {}
    handler
        .handle_attached_live_input_for_test(attach_pid, b"y")
        .await
        .expect("confirmation input succeeds");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match control_rx.recv().await {
                Some(AttachControl::Overlay(_)) => break,
                Some(_) => {}
                None => panic!("attach control channel closed before action refresh"),
            }
        }
    })
    .await
    .expect("the confirmed action refreshes the mode-tree overlay");
}

#[path = "tests/parse_and_tag.rs"]
mod parse_and_tag;

#[path = "tests/render_items.rs"]
mod render_items;

#[path = "tests/selection_scroll.rs"]
mod selection_scroll;

#[path = "tests/preview_rendering.rs"]
mod preview_rendering;

#[path = "tests/tags_search_parse.rs"]
mod tags_search_parse;

#[path = "tests/tree_navigation.rs"]
mod tree_navigation;

#[path = "tests/async_acceptance.rs"]
mod async_acceptance;

#[path = "tests/window_occurrence_identity.rs"]
mod window_occurrence_identity;

#[path = "tests/client_identity.rs"]
mod client_identity;

#[path = "tests/deferred_confirmation.rs"]
mod deferred_confirmation;

#[path = "tests/switch_geometry.rs"]
mod switch_geometry;

#[path = "tests/status_geometry.rs"]
mod status_geometry;
