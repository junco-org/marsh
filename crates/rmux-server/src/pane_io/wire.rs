use std::future::pending;
use std::io;

use rmux_core::events::{OutputCursorItem, OutputGap};
use rmux_proto::{
    encode_attach_data, encode_attach_data_into_slice, encode_attach_message, AttachFrameDecoder,
    AttachMessage, ATTACH_DATA_HEADER_LEN, DEFAULT_MAX_FRAME_LENGTH,
};
use tracing::warn;

use crate::outer_terminal::OuterTerminal;

use super::attach_transport::{AttachTransport, TryAttachRead};
use super::types::{AttachTarget, OpenAttachTarget, PaneOutputReceiver};

const STACK_ATTACH_DATA_PAYLOAD: usize = 1024;

pub(super) fn open_attach_target(
    target: AttachTarget,
    render_stream: bool,
) -> io::Result<OpenAttachTarget> {
    let AttachTarget {
        session_name,
        live_pane_handover: _,
        pane_output,
        pane_output_start_sequence: _,
        render_frame,
        outer_terminal,
        client_title: _,
        cursor_style,
        active_pane_geometry,
        raw_passthrough,
        kitty_graphics_passthrough,
        sixel_passthrough,
        persistent_overlay_state_id,
        live_pane,
    } = target;
    Ok(OpenAttachTarget {
        session_name,
        predicted_echo: Default::default(),
        predicted_echo_started_at: None,
        pane_output: Some(pane_output),
        render_frame,
        outer_terminal,
        cursor_style,
        active_pane_geometry,
        raw_passthrough,
        kitty_graphics_passthrough,
        sixel_passthrough,
        persistent_overlay_state_id,
        live_pane,
        render_stream,
    })
}

pub(super) async fn emit_render_frame(
    stream: &AttachTransport,
    outer_terminal: &OuterTerminal,
    render_frame: &[u8],
) -> io::Result<()> {
    let Some(frame) = bounded_wrapped_render_frame(outer_terminal, render_frame) else {
        return emit_attach_bytes(stream, render_frame).await;
    };
    emit_attach_bytes(stream, &frame).await
}

pub(super) async fn emit_coalescible_render_frame(
    stream: &AttachTransport,
    outer_terminal: &OuterTerminal,
    render_frame: &[u8],
    render_stream: bool,
) -> io::Result<()> {
    if render_frame.is_empty() {
        return Ok(());
    }
    let Some(frame) = bounded_wrapped_render_frame(outer_terminal, render_frame) else {
        // Render messages are replaceable, so fragmenting one would let the
        // client discard an intermediate part. Keep a still-bounded repaint
        // replaceable without synchronized-output wrapping; larger repaints
        // use strict Data messages whose order the client must preserve.
        return if render_stream && render_frame.len() <= DEFAULT_MAX_FRAME_LENGTH {
            emit_attach_frame(stream, &AttachMessage::Render(render_frame.to_vec())).await
        } else {
            emit_attach_bytes(stream, render_frame).await
        };
    };
    if render_stream {
        emit_attach_frame(stream, &AttachMessage::Render(frame)).await
    } else {
        emit_attach_bytes(stream, &frame).await
    }
}

fn bounded_wrapped_render_frame(
    outer_terminal: &OuterTerminal,
    render_frame: &[u8],
) -> Option<Vec<u8>> {
    if render_frame.len() > DEFAULT_MAX_FRAME_LENGTH {
        return None;
    }
    let frame = outer_terminal.wrap_render_frame(render_frame);
    (frame.len() <= DEFAULT_MAX_FRAME_LENGTH).then_some(frame)
}

pub(super) async fn read_socket_bytes(
    stream: &AttachTransport,
    decoder: &mut AttachFrameDecoder,
) -> io::Result<bool> {
    stream.read_into(decoder).await
}

pub(super) fn try_read_socket_bytes(
    stream: &AttachTransport,
    decoder: &mut AttachFrameDecoder,
) -> io::Result<TryAttachRead> {
    stream.try_read_into(decoder)
}

pub(super) async fn emit_attach_message(
    stream: &AttachTransport,
    message: &AttachMessage,
) -> io::Result<()> {
    let frame = encode_attach_message(message).map_err(io::Error::other)?;
    write_all_to_stream(stream, &frame).await
}

pub(super) async fn emit_attach_frame(
    stream: &AttachTransport,
    message: &AttachMessage,
) -> io::Result<()> {
    let frame = encode_attach_message(message).map_err(io::Error::other)?;
    write_all_to_stream(stream, &frame).await
}

pub(super) async fn recv_pane_output(
    pane_output: &mut PaneOutputReceiver,
) -> io::Result<OutputCursorItem> {
    match pane_output.recv().await {
        OutputCursorItem::Event(event) => Ok(OutputCursorItem::Event(event)),
        OutputCursorItem::Gap(gap) => {
            warn_pane_output_gap(&gap);
            Ok(OutputCursorItem::Gap(gap))
        }
    }
}

pub(super) fn warn_pane_output_gap(gap: &OutputGap) {
    warn!(
        expected_sequence = gap.expected_sequence(),
        resume_sequence = gap.resume_sequence(),
        missed_events = gap.missed_events(),
        recent_bytes = gap.recent_snapshot().len(),
        "attach pane output receiver lagged"
    );
}

pub(super) async fn recv_pane_output_optional(
    pane_output: Option<&mut PaneOutputReceiver>,
) -> io::Result<Option<OutputCursorItem>> {
    match pane_output {
        Some(pane_output) => recv_pane_output(pane_output).await.map(Some),
        None => pending().await,
    }
}

pub(super) async fn emit_attach_data_frame(
    stream: &AttachTransport,
    bytes: &[u8],
) -> io::Result<()> {
    if bytes.len() <= STACK_ATTACH_DATA_PAYLOAD {
        let mut frame = [0_u8; STACK_ATTACH_DATA_PAYLOAD + ATTACH_DATA_HEADER_LEN];
        let len = encode_attach_data_into_slice(bytes, &mut frame).map_err(io::Error::other)?;
        return write_all_to_stream(stream, &frame[..len]).await;
    }

    let frame = encode_attach_data(bytes).map_err(io::Error::other)?;
    write_all_to_stream(stream, &frame).await
}

pub(super) async fn emit_attach_bytes(stream: &AttachTransport, bytes: &[u8]) -> io::Result<()> {
    // Data frames are strict stream fragments on both attach clients. The
    // fixed codec ceiling bounds each encoder allocation independently of the
    // rendered keyframe or passthrough event that supplied these bytes.
    for chunk in bytes.chunks(DEFAULT_MAX_FRAME_LENGTH) {
        emit_attach_data_frame(stream, chunk).await?;
    }
    Ok(())
}

pub(super) async fn emit_attach_stop(
    stream: &AttachTransport,
    current_target: &OpenAttachTarget,
) -> io::Result<()> {
    emit_attach_bytes(
        stream,
        &current_target.outer_terminal.attach_stop_sequence(),
    )
    .await
}

pub(super) async fn emit_detached_attach_stop(
    stream: &AttachTransport,
    current_target: &OpenAttachTarget,
) -> io::Result<()> {
    let mut bytes = current_target.outer_terminal.attach_stop_sequence();
    bytes.extend_from_slice(
        format!(
            "[detached (from session {})]\r\n",
            current_target.session_name
        )
        .as_bytes(),
    );
    emit_attach_bytes(stream, &bytes).await
}

pub(super) async fn emit_exited_attach_stop(
    stream: &AttachTransport,
    current_target: &OpenAttachTarget,
) -> io::Result<()> {
    let mut bytes = current_target.outer_terminal.attach_stop_sequence();
    bytes.extend_from_slice(b"[exited]\r\n");
    emit_attach_bytes(stream, &bytes).await
}


async fn write_all_to_stream(stream: &AttachTransport, bytes: &[u8]) -> io::Result<()> {
    stream.write_all(bytes).await
}

pub(super) fn invalid_attach_message(error: rmux_proto::RmuxError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod emit_render_tests {
    use super::{emit_attach_bytes, emit_attach_message, emit_coalescible_render_frame};
    use crate::outer_terminal::{OuterTerminal, OuterTerminalContext};
    use crate::pane_io::attach_transport::AttachTransport;
    use rmux_core::OptionStore;
    use rmux_proto::{
        AttachFrameDecoder, AttachMessage, AttachShellCommand, OptionName, ScopeSelector,
        SetOptionMode, DEFAULT_MAX_FRAME_LENGTH,
    };
    use tokio::io::AsyncReadExt;

    async fn emitted_messages(
        outer_terminal: &OuterTerminal,
        render_frame: &[u8],
        render_stream: bool,
        expected_count: usize,
    ) -> Vec<AttachMessage> {
        let capacity = render_frame
            .len()
            .saturating_add(expected_count.saturating_mul(5))
            .saturating_add(64);
        let (stream, mut peer) = tokio::io::duplex(capacity);
        let stream = AttachTransport::from_io(stream);

        emit_coalescible_render_frame(&stream, outer_terminal, render_frame, render_stream)
            .await
            .expect("render frame emits");

        let mut decoder = AttachFrameDecoder::new();
        let mut messages = Vec::with_capacity(expected_count);
        let mut bytes = [0_u8; 8192];
        while messages.len() < expected_count {
            let count = peer.read(&mut bytes).await.expect("read emitted frame");
            assert!(count > 0, "attach stream closed before all frames arrived");
            decoder.push_bytes(&bytes[..count]);
            while let Some(message) = decoder.next_message().expect("decode emitted frame") {
                messages.push(message);
            }
        }
        messages
    }

    async fn emitted_message(render_stream: bool) -> AttachMessage {
        let outer_terminal =
            OuterTerminal::resolve(&OptionStore::default(), OuterTerminalContext::default());
        emitted_messages(&outer_terminal, b"frame", render_stream, 1)
            .await
            .pop()
            .expect("message emitted")
    }

    #[tokio::test]
    async fn coalescible_render_falls_back_to_data_for_legacy_clients() {
        assert!(matches!(
            emitted_message(false).await,
            AttachMessage::Data(bytes) if bytes.ends_with(b"frame")
        ));
    }

    #[tokio::test]
    async fn coalescible_render_uses_render_for_capable_clients() {
        assert!(matches!(
            emitted_message(true).await,
            AttachMessage::Render(bytes) if bytes.ends_with(b"frame")
        ));
    }

    #[tokio::test]
    async fn coalescible_render_fragments_strictly_above_attach_payload_ceiling() {
        let outer_terminal =
            OuterTerminal::resolve(&OptionStore::default(), OuterTerminalContext::default());
        let at_limit = vec![b'a'; DEFAULT_MAX_FRAME_LENGTH];
        let above_limit = vec![b'b'; DEFAULT_MAX_FRAME_LENGTH + 1];

        assert_eq!(
            emitted_messages(&outer_terminal, &at_limit, true, 1).await,
            vec![AttachMessage::Render(at_limit)]
        );
        assert_eq!(
            emitted_messages(&outer_terminal, &above_limit, true, 2).await,
            vec![
                AttachMessage::Data(vec![b'b'; DEFAULT_MAX_FRAME_LENGTH]),
                AttachMessage::Data(vec![b'b']),
            ]
        );
        assert_eq!(
            emitted_messages(&outer_terminal, &above_limit, false, 2).await,
            vec![
                AttachMessage::Data(vec![b'b'; DEFAULT_MAX_FRAME_LENGTH]),
                AttachMessage::Data(vec![b'b']),
            ]
        );
    }

    #[tokio::test]
    async fn fragmented_attach_data_bounds_every_payload_and_preserves_order() {
        let mut payload = (0..(2 * DEFAULT_MAX_FRAME_LENGTH + 17))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let split_sequence = b"\x1b[38;2;1;2;3mX";
        let split_start = DEFAULT_MAX_FRAME_LENGTH - 2;
        payload[split_start..split_start + split_sequence.len()].copy_from_slice(split_sequence);
        let capacity = payload.len() + 3 * 5;
        let (stream, mut peer) = tokio::io::duplex(capacity);
        let stream = AttachTransport::from_io(stream);

        emit_attach_bytes(&stream, &payload)
            .await
            .expect("fragmented data emits");

        let mut decoder = AttachFrameDecoder::new();
        let mut decoded = Vec::with_capacity(payload.len());
        let mut lengths = Vec::new();
        let mut bytes = [0_u8; 8192];
        while decoded.len() < payload.len() {
            let count = peer.read(&mut bytes).await.expect("read emitted data");
            assert!(count > 0, "attach stream closed before all data arrived");
            decoder.push_bytes(&bytes[..count]);
            while let Some(message) = decoder.next_message().expect("decode emitted data") {
                let AttachMessage::Data(bytes) = message else {
                    panic!("fragmented terminal bytes must use strict data frames");
                };
                lengths.push(bytes.len());
                decoded.extend_from_slice(&bytes);
            }
        }

        assert_eq!(
            lengths,
            vec![DEFAULT_MAX_FRAME_LENGTH, DEFAULT_MAX_FRAME_LENGTH, 17]
        );
        assert_eq!(decoded, payload);
    }

    #[tokio::test]
    async fn sync_wrapper_degrades_without_splitting_replaceable_render() {
        let mut options = OptionStore::new();
        options
            .set(
                ScopeSelector::Global,
                OptionName::TerminalFeatures,
                "xterm*:sync".to_owned(),
                SetOptionMode::Append,
            )
            .expect("terminal-features append succeeds");
        let outer_terminal = OuterTerminal::resolve(
            &options,
            OuterTerminalContext::from_pairs(&[("TERM", "xterm-256color")]),
        );
        let sync_overhead = outer_terminal.wrap_render_frame(b"x").len() - 1;
        let at_limit = vec![b'a'; DEFAULT_MAX_FRAME_LENGTH - sync_overhead];
        let above_limit = vec![b'b'; DEFAULT_MAX_FRAME_LENGTH - sync_overhead + 1];

        let exact = emitted_messages(&outer_terminal, &at_limit, true, 1).await;
        let AttachMessage::Render(exact) = &exact[0] else {
            panic!("an exactly bounded synchronized repaint stays replaceable");
        };
        assert_eq!(exact.len(), DEFAULT_MAX_FRAME_LENGTH);
        assert!(exact.starts_with(b"\x1b[?2026h"));
        assert!(exact.ends_with(b"\x1b[?2026l"));

        assert_eq!(
            emitted_messages(&outer_terminal, &above_limit, true, 1).await,
            vec![AttachMessage::Render(above_limit)]
        );
    }

    #[tokio::test]
    async fn attach_control_messages_are_not_wrapped_as_terminal_data() {
        let (stream, mut peer) = tokio::io::duplex(1024);
        let stream = AttachTransport::from_io(stream);
        let message = AttachMessage::DetachExecShellCommand(AttachShellCommand::new(
            "echo detached".to_owned(),
            "/bin/sh".to_owned(),
            "/tmp".to_owned(),
        ));

        emit_attach_message(&stream, &message)
            .await
            .expect("attach message emits");

        let mut bytes = [0_u8; 256];
        let count = peer.read(&mut bytes).await.expect("read emitted frame");
        let mut decoder = AttachFrameDecoder::new();
        decoder.push_bytes(&bytes[..count]);
        assert_eq!(
            decoder.next_message().expect("decode succeeds"),
            Some(message)
        );
    }
}
