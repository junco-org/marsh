use std::path::Path;

use rmux_proto::{PaneSnapshotCell, PaneSnapshotResponse};
use serde_json::{Value, json};

use crate::cli_args::{PaneSnapshotArgs, SnapshotRegion};

use super::super::ExitFailure;
use super::super::target_resolution::connect_cli;
use super::common::{
    SCHEMA_VERSION, check_disabled, pane_snapshot, resolve_pane_ref, visible_line_from_cells,
    write_json_line, write_stdout_line,
};

/// Runs `pane-snapshot`, printing the requested region as text or as a JSON grid.
pub(crate) fn run_pane_snapshot(
    args: &PaneSnapshotArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    check_disabled("RMUX_DISABLE_PANE_SNAPSHOT", "pane-snapshot")?;
    let mut connection = connect_cli(socket_path)?;
    let target = resolve_pane_ref(&mut connection, args.target.as_ref(), "pane-snapshot")?;
    let snapshot = pane_snapshot(&mut connection, target)?;
    let view = SnapshotView::new(&snapshot, args.region)?;
    if args.json {
        return write_json_line(&snapshot_json(&snapshot, &view, args.style));
    }
    write_stdout_line(&view.lines.join("\n"))
}

/// The rectangular part of a pane snapshot a command asked for, as text lines and cells.
struct SnapshotView {
    row: u16,
    col: u16,
    rows: u16,
    cols: u16,
    lines: Vec<String>,
    cells: Vec<Vec<PaneSnapshotCell>>,
}

impl SnapshotView {
    /// Cuts the requested region out of the snapshot grid, defaulting to the whole pane.
    fn new(
        snapshot: &PaneSnapshotResponse,
        region: Option<SnapshotRegion>,
    ) -> Result<Self, ExitFailure> {
        let region = region.unwrap_or(SnapshotRegion {
            row: 0,
            col: 0,
            rows: snapshot.rows,
            cols: snapshot.cols,
        });
        validate_region(snapshot, region)?;
        let mut lines = Vec::with_capacity(usize::from(region.rows));
        let mut cells = Vec::with_capacity(usize::from(region.rows));
        let snapshot_cols = usize::from(snapshot.cols);
        for row_offset in 0..usize::from(region.rows) {
            let row = usize::from(region.row) + row_offset;
            let col = usize::from(region.col);
            let end_col = col + usize::from(region.cols);
            let start = row * snapshot_cols + col;
            let end = row * snapshot_cols + end_col;
            let row_cells = snapshot.cells[start..end].to_vec();
            lines.push(visible_line_from_cells(&row_cells));
            cells.push(row_cells);
        }
        Ok(Self {
            row: region.row,
            col: region.col,
            rows: region.rows,
            cols: region.cols,
            lines,
            cells,
        })
    }
}

/// Rejects a truncated cell grid or a region reaching past the pane's rows and columns.
fn validate_region(
    snapshot: &PaneSnapshotResponse,
    region: SnapshotRegion,
) -> Result<(), ExitFailure> {
    let expected_cells = usize::from(snapshot.rows).saturating_mul(usize::from(snapshot.cols));
    if snapshot.cells.len() < expected_cells {
        return Err(ExitFailure::new(
            1,
            "pane-snapshot response has an incomplete cell grid",
        ));
    }
    let row_end = u32::from(region.row) + u32::from(region.rows);
    let col_end = u32::from(region.col) + u32::from(region.cols);
    if row_end > u32::from(snapshot.rows) || col_end > u32::from(snapshot.cols) {
        return Err(ExitFailure::new(
            1,
            "pane-snapshot --region exceeds pane bounds",
        ));
    }
    Ok(())
}

/// Builds the `pane-snapshot` JSON payload, adding per-cell styling when `include_style` is set.
fn snapshot_json(
    snapshot: &PaneSnapshotResponse,
    view: &SnapshotView,
    include_style: bool,
) -> Value {
    let mut payload = json!({
        "schema_version": SCHEMA_VERSION,
        "ok": true,
        "rows": snapshot.rows,
        "cols": snapshot.cols,
        "revision": snapshot.revision,
        "cursor": {
            "row": snapshot.cursor.row,
            "col": snapshot.cursor.col,
            "visible": snapshot.cursor.visible,
            "style": snapshot.cursor.style,
        },
        "region": {
            "row": view.row,
            "col": view.col,
            "rows": view.rows,
            "cols": view.cols,
        },
        "text": view.lines.join("\n"),
        "lines": view.lines,
    });
    if include_style {
        payload["cells"] = Value::Array(
            view.cells
                .iter()
                .map(|row| {
                    Value::Array(
                        row.iter()
                            .map(|cell| {
                                json!({
                                    "text": cell.text,
                                    "width": cell.width,
                                    "padding": cell.padding,
                                    "attributes": cell.attributes,
                                    "fg": cell.fg,
                                    "bg": cell.bg,
                                    "us": cell.us,
                                    "link": cell.link,
                                })
                            })
                            .collect(),
                    )
                })
                .collect(),
        );
    }
    payload
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use crate::cli_args::SnapshotRegion;

    use super::super::common::fixtures::wide_glyph_snapshot;
    use super::SnapshotView;

    #[test]
    fn region_text_uses_terminal_cell_columns_without_leaking_wide_glyphs() {
        // A region starting on the wide glyph's padding cell must not leak the glyph itself.
        for (col, cols, expected) in [(1, 3, "界B"), (2, 2, "B")] {
            let snapshot = wide_glyph_snapshot();
            let region = SnapshotRegion {
                row: 0,
                col,
                rows: 1,
                cols,
            };
            let view = SnapshotView::new(&snapshot, Some(region)).expect("region is valid");

            assert_eq!(view.lines, vec![expected], "region at column {col}");
        }
    }

    #[test]
    fn incomplete_snapshot_grid_is_rejected_before_slicing() {
        let mut snapshot = wide_glyph_snapshot();
        snapshot.cells.pop();

        let Err(error) = SnapshotView::new(&snapshot, None) else {
            panic!("grid is incomplete")
        };

        assert!(
            error
                .message()
                .contains("pane-snapshot response has an incomplete cell grid"),
            "{}",
            error.message()
        );
    }
}
