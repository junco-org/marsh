use qrcode::{Color as QrColor, EcLevel, QrCode};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

const QUIET_ZONE: usize = 4;
const QR_LIGHT: Color = Color::Indexed(15);

/// Selects how QR modules are drawn: half blocks, doubled blocks, or styled background cells.
#[derive(Clone, Copy)]
pub(super) enum RenderMode {
    Compact,
    Plain,
    TerminalSafe,
}

/// Encodes `data` as a QR code and renders it as ratatui lines in the requested `mode`.
pub(super) fn render_lines(
    data: &str,
    mode: RenderMode,
) -> Result<Vec<Line<'static>>, qrcode::types::QrError> {
    let rows = qr_rows(data)?;
    Ok(match mode {
        RenderMode::Compact => half_block_lines(&rows),
        RenderMode::Plain => plain_lines(&rows),
        RenderMode::TerminalSafe => terminal_safe_lines(&rows),
    })
}

/// Columns the rendered QR code occupies, or `0` when `data` cannot be encoded.
pub(super) fn width(data: &str, mode: RenderMode) -> usize {
    qr_rows(data).map_or(0, |rows| {
        let modules = rows.first().map_or(0, Vec::len);
        match mode {
            RenderMode::Compact => modules,
            RenderMode::Plain | RenderMode::TerminalSafe => modules.saturating_mul(2),
        }
    })
}

/// Terminal rows the rendered QR code occupies, or `0` when `data` cannot be encoded.
pub(super) fn height(data: &str, mode: RenderMode) -> usize {
    qr_rows(data).map_or(0, |rows| match mode {
        RenderMode::Compact => rows.len().div_ceil(2),
        RenderMode::Plain | RenderMode::TerminalSafe => rows.len(),
    })
}

/// Encodes `data` at error-correction level `L` into a dark/light module grid with a quiet zone.
fn qr_rows(data: &str) -> Result<Vec<Vec<bool>>, qrcode::types::QrError> {
    let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::L)?;
    let size = code.width() + QUIET_ZONE * 2;
    let mut rows = Vec::new();

    for y in 0..size {
        let mut row = Vec::with_capacity(size);
        for x in 0..size {
            row.push(qr_dark(&code, x, y));
        }
        rows.push(row);
    }

    Ok(rows)
}

/// Packs each pair of module rows into one line of half-block glyphs, black on light.
fn half_block_lines(rows: &[Vec<bool>]) -> Vec<Line<'static>> {
    rows.chunks(2)
        .map(|pair| {
            let top = pair[0].as_slice();
            let bottom = pair.get(1).map(Vec::as_slice);
            let mut line = String::new();
            for (x, top_dark) in top.iter().copied().enumerate() {
                let bottom_dark = bottom.and_then(|row| row.get(x)).copied().unwrap_or(false);
                line.push(match (top_dark, bottom_dark) {
                    (false, false) => ' ',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (true, true) => '█',
                });
            }
            Line::from(Span::styled(
                line,
                Style::default().fg(Color::Black).bg(QR_LIGHT),
            ))
        })
        .collect()
}

/// Renders one line per module row using styled background cells, for terminals lacking blocks.
fn terminal_safe_lines(rows: &[Vec<bool>]) -> Vec<Line<'static>> {
    rows.iter().map(|row| full_cell_line(row)).collect()
}

/// Renders each module row as two unstyled block characters per module, for copyable output.
fn plain_lines(rows: &[Vec<bool>]) -> Vec<Line<'static>> {
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for dark in row {
                if *dark {
                    line.push_str("██");
                } else {
                    line.push_str("  ");
                }
            }
            Line::from(line)
        })
        .collect()
}

/// Builds one line from a module row, coalescing equal runs into background-styled blank spans.
fn full_cell_line(row: &[bool]) -> Line<'static> {
    let mut spans = Vec::new();
    let mut index = 0;
    while index < row.len() {
        let dark = row[index];
        let start = index;
        while index < row.len() && row[index] == dark {
            index += 1;
        }
        let cells = index.saturating_sub(start).saturating_mul(2);
        spans.push(Span::styled(
            " ".repeat(cells),
            Style::default().bg(if dark { Color::Black } else { QR_LIGHT }),
        ));
    }
    Line::from(spans)
}

/// Reports whether the module at `x`/`y` is dark, treating the quiet-zone border as light.
fn qr_dark(code: &QrCode, x: usize, y: usize) -> bool {
    if x < QUIET_ZONE || y < QUIET_ZONE {
        return false;
    }
    let qx = x - QUIET_ZONE;
    let qy = y - QUIET_ZONE;
    qx < code.width() && qy < code.width() && code[(qx, qy)] == QrColor::Dark
}
