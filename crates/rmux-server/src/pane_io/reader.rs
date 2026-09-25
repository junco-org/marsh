use rmux_core::{PaneId, TerminalPassthrough, TerminalPassthroughKind};
use std::sync::{mpsc as std_mpsc, Arc, OnceLock};
use std::time::Instant;
use tracing::warn;

use super::{PaneAlertCallback, PaneAlertEvent, PaneOutputSender};
use crate::clipboard_protocol::decode_pane_clipboard_write_payload;
use crate::pane_transcript::{PaneGroundTimer, SharedPaneTranscript};

struct PanePublishContext<'a> {
    session_name: &'a rmux_proto::SessionName,
    pane_id: PaneId,
    transcript: &'a SharedPaneTranscript,
    pane_output: &'a PaneOutputSender,
    generation: Option<u64>,
    pane_alert_callback: Option<&'a PaneAlertCallback>,
    emit_no_bell_alert: bool,
}

fn publish_pane_bytes(context: PanePublishContext<'_>, bytes: Vec<u8>) -> Vec<u8> {
    let PanePublishContext {
        session_name,
        pane_id,
        transcript,
        pane_output,
        generation,
        pane_alert_callback,
        emit_no_bell_alert,
    } = context;
    if !pane_output.accepts_generation(generation) {
        return Vec::new();
    }
    let Some((_sequence, append_result)) =
        pane_output.publish_for_generation_with_invalidation(generation, bytes, |bytes| {
            let mut transcript = transcript
                .lock()
                .expect("pane transcript mutex must not be poisoned");
            let mouse_mode_before = transcript.mode() & rmux_core::input::mode::ALL_MOUSE_MODES;
            let mut append_result = transcript.append_bytes_with_effects(bytes);
            let mouse_mode_changed =
                transcript.mode() & rmux_core::input::mode::ALL_MOUSE_MODES != mouse_mode_before;
            let passthroughs = std::mem::take(&mut append_result.passthroughs);
            let clipboard_set = passthroughs.iter().any(passthrough_is_clipboard_set);
            let clipboard_writes = passthroughs
                .iter()
                .filter_map(osc52_clipboard_write_payload)
                .collect::<Vec<_>>();
            let clipboard_queries = passthroughs
                .iter()
                .filter_map(TerminalPassthrough::clipboard_query_metadata)
                .collect::<Vec<_>>();
            if let Some(callback) = pane_alert_callback {
                callback(PaneAlertEvent {
                    session_name: session_name.clone(),
                    pane_id,
                    bell_count: append_result.bell_count,
                    title_changed: append_result.title_changed,
                    title_change: append_result.title_change.clone(),
                    path_changed: append_result.path_changed,
                    clipboard_set,
                    clipboard_writes,
                    clipboard_queries,
                    mouse_mode_changed,
                    alternate_mode_changed: append_result.alternate_mode_changed,
                    queue_activity_alert: emit_no_bell_alert || append_result.bell_count > 0,
                    generation,
                });
            }
            let invalidation = append_result
                .recovery_rebase_required
                .then_some(super::PaneInvalidationReason::TranscriptMutation);
            (append_result, passthroughs, invalidation)
        })
    else {
        return Vec::new();
    };
    if let Some(timer) = append_result.ground_timer {
        schedule_pane_ground_timer(
            session_name,
            pane_id,
            Arc::clone(transcript),
            pane_output.clone(),
            timer,
        );
    }
    let replies = append_result.replies;
    let dropped_passthrough_count = append_result.dropped_passthrough_count;
    if dropped_passthrough_count > 0 {
        warn!(
            session = %session_name,
            pane_id = pane_id.as_u32(),
            dropped = dropped_passthrough_count,
            "dropped terminal passthrough events due to parser safety limits"
        );
    }
    replies
}

/// Publishes one chunk of a ShellMux-backed job's terminal stream into the pane that presents it.
///
/// The same transcript, ring, alert and passthrough path a pseudoterminal pane reader uses — the
/// only difference is who produced the bytes. Routing shell output anywhere else would give the
/// daemon a second fan-out with its own idea of sequencing, alerts and recovery invalidation.
///
/// The returned bytes are the terminal's replies to queries the program sent (a primary device
/// attribute request, a palette query). They belong back in that job's input; the caller owns
/// delivering them through the managed input path, because the mux — not this function — owns
/// write ordering and input closure.
///
/// Published unguarded (`generation: None`): a shell job's route records the session and the
/// stable pane id, and the job's own identity is the generation. A pane whose route still points
/// at this job is by construction presenting it.
pub(crate) fn publish_shell_pane_bytes(
    session_name: &rmux_proto::SessionName,
    pane_id: PaneId,
    transcript: &SharedPaneTranscript,
    pane_output: &PaneOutputSender,
    pane_alert_callback: Option<&PaneAlertCallback>,
    bytes: Vec<u8>,
) -> Vec<u8> {
    publish_pane_bytes(
        PanePublishContext {
            session_name,
            pane_id,
            transcript,
            pane_output,
            generation: None,
            pane_alert_callback,
            emit_no_bell_alert: true,
        },
        bytes,
    )
}

/// Publishes bytes through the production pane-output path for one real pane
/// and returns the alert events production built from them.
///
/// Tests that need the handler side of an alert drive it with these events
/// rather than a hand-written one, so what the parser reported and what the
/// handler acts on cannot drift apart.
#[cfg(test)]
pub(crate) fn publish_pane_bytes_capturing_alerts(
    session_name: &rmux_proto::SessionName,
    pane_id: PaneId,
    transcript: &SharedPaneTranscript,
    pane_output: &PaneOutputSender,
    bytes: Vec<u8>,
) -> Vec<super::PaneAlertEvent> {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    {
        let callback: super::PaneAlertCallback = Arc::new(move |event| {
            sink.lock()
                .expect("pane alert capture mutex must not be poisoned")
                .push(event);
        });
        let _ = publish_pane_bytes(
            PanePublishContext {
                session_name,
                pane_id,
                transcript,
                pane_output,
                generation: None,
                pane_alert_callback: Some(&callback),
                emit_no_bell_alert: false,
            },
            bytes,
        );
    }
    let captured = std::mem::take(
        &mut *events
            .lock()
            .expect("pane alert capture mutex must not be poisoned"),
    );
    captured
}

#[cfg(test)]
pub(crate) fn publish_pane_bytes_for_test(
    transcript: &SharedPaneTranscript,
    pane_output: &PaneOutputSender,
    bytes: Vec<u8>,
) {
    let session_name =
        rmux_proto::SessionName::new("pane-output-test").expect("test session name must be valid");
    let _ = publish_pane_bytes(
        PanePublishContext {
            session_name: &session_name,
            pane_id: PaneId::new(1),
            transcript,
            pane_output,
            generation: None,
            pane_alert_callback: None,
            emit_no_bell_alert: false,
        },
        bytes,
    );
}

fn passthrough_is_clipboard_set(passthrough: &TerminalPassthrough) -> bool {
    passthrough.kind() == TerminalPassthroughKind::Clipboard
        && osc52_payload_is_clipboard_set(passthrough.payload())
}

fn osc52_payload_is_clipboard_set(sequence: &[u8]) -> bool {
    let Some(body) = sequence.strip_prefix(b"\x1b]52;") else {
        return false;
    };
    let Some(body) = body
        .strip_suffix(b"\x07")
        .or_else(|| body.strip_suffix(b"\x1b\\"))
    else {
        return false;
    };
    let Some(separator) = body.iter().position(|byte| *byte == b';') else {
        return false;
    };
    let payload = &body[separator + 1..];
    if payload.is_empty() || payload == b"?" {
        return false;
    }
    // Use the same strict decoder as `osc52_clipboard_write_payload` so the
    // clipboard_set flag (which drives the PaneSetClipboard hook emission)
    // stays in sync with buffer storage. Base64 payloads that decode to zero
    // bytes (e.g. a lone `==` sequence) match the syntax-only classifier but
    // are dropped by paste_add on the frozen tmux 3.7b oracle — mirror that
    // so the hook does not fire without a stored buffer.
    osc52_payload_decodes(payload)
}

/// Decodes an inbound OSC 52 clipboard-write passthrough to its raw bytes, or
/// returns None for a query (`?`), an empty/malformed payload, or a
/// non-clipboard passthrough. Matches the frozen tmux 3.7b oracle: empty and
/// invalid-base64 writes create no paste buffer (input_osc_52 returns early
/// without paste_add).
fn osc52_clipboard_write_payload(passthrough: &TerminalPassthrough) -> Option<Vec<u8>> {
    if passthrough.kind() != TerminalPassthroughKind::Clipboard {
        return None;
    }
    let body = passthrough.payload().strip_prefix(b"\x1b]52;")?;
    let body = body
        .strip_suffix(b"\x07")
        .or_else(|| body.strip_suffix(b"\x1b\\"))?;
    let separator = body.iter().position(|byte| *byte == b';')?;
    let payload = &body[separator + 1..];
    if payload.is_empty() || payload == b"?" {
        return None;
    }
    let decoded = decode_pane_clipboard_write_payload(payload)?;
    // A decoded length of 0 is possible when the base64 symbols round to zero
    // output bytes (e.g. a lone `=` sequence). tmux drops these too.
    if decoded.is_empty() {
        return None;
    }
    Some(decoded)
}

/// Same validation as `osc52_clipboard_write_payload` reduced to a boolean,
/// exposed to the outer-forward gate in `passthrough.rs`. Kept in this module
/// so both paths share the exact same decoder.
pub(super) fn osc52_payload_decodes(payload: &[u8]) -> bool {
    decode_pane_clipboard_write_payload(payload)
        .map(|decoded| !decoded.is_empty())
        .unwrap_or(false)
}

struct PaneGroundTimerJob {
    transcript: SharedPaneTranscript,
    pane_output: PaneOutputSender,
    timer: PaneGroundTimer,
}

fn schedule_pane_ground_timer(
    session_name: &rmux_proto::SessionName,
    pane_id: PaneId,
    transcript: SharedPaneTranscript,
    pane_output: PaneOutputSender,
    timer: PaneGroundTimer,
) {
    let job = PaneGroundTimerJob {
        transcript,
        pane_output,
        timer,
    };
    if let Err(error) = pane_ground_timer_tx().send(job) {
        warn!(
            session = %session_name,
            pane_id = pane_id.as_u32(),
            "failed to schedule pane parser ground timer: {error}"
        );
    }
}

fn pane_ground_timer_tx() -> &'static std_mpsc::Sender<PaneGroundTimerJob> {
    static TIMER_TX: OnceLock<std_mpsc::Sender<PaneGroundTimerJob>> = OnceLock::new();
    TIMER_TX.get_or_init(|| {
        let (tx, rx) = std_mpsc::channel();
        spawn_pane_ground_timer_worker(rx);
        tx
    })
}

fn spawn_pane_ground_timer_worker(rx: std_mpsc::Receiver<PaneGroundTimerJob>) {
    let thread_name = "rmux-pane-ground-timer".to_owned();
    if let Err(error) = std::thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || run_pane_ground_timer_worker(rx))
    {
        warn!(
            thread = %thread_name,
            "failed to spawn pane parser ground timer worker: {error}"
        );
    }
}

fn run_pane_ground_timer_worker(rx: std_mpsc::Receiver<PaneGroundTimerJob>) {
    let mut jobs = Vec::<PaneGroundTimerJob>::new();
    loop {
        if jobs.is_empty() {
            match rx.recv() {
                Ok(job) => {
                    jobs.push(job);
                    continue;
                }
                Err(_) => return,
            }
        }

        expire_due_pane_ground_timers(&mut jobs);
        if jobs.is_empty() {
            continue;
        }

        let next_deadline = jobs
            .iter()
            .map(|job| job.timer.deadline)
            .min()
            .expect("non-empty timer job queue has a deadline");
        let timeout = next_deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout) {
            Ok(job) => jobs.push(job),
            Err(std_mpsc::RecvTimeoutError::Timeout) => expire_due_pane_ground_timers(&mut jobs),
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn expire_due_pane_ground_timers(jobs: &mut Vec<PaneGroundTimerJob>) {
    let now = Instant::now();
    let mut index = 0;
    while index < jobs.len() {
        if now < jobs[index].timer.deadline {
            index += 1;
            continue;
        }
        let job = jobs.swap_remove(index);
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            expire_pane_ground_timer_job(job);
        }))
        .is_err()
        {
            warn!("pane parser ground timer job panicked; timer worker is continuing");
        }
    }
}

fn expire_pane_ground_timer_job(job: PaneGroundTimerJob) {
    job.pane_output.mutate_transcript(
        &job.transcript,
        super::PaneInvalidationReason::ParserStateExpired,
        |transcript| {
            let expired = transcript.expire_ground_timer(job.timer);
            ((), expired)
        },
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use super::{
        osc52_clipboard_write_payload, osc52_payload_is_clipboard_set, publish_pane_bytes,
        PanePublishContext,
    };
    use rmux_core::{
        input::InputEndType, PaneId, TerminalClipboardQuery, TerminalPassthrough,
        TerminalPassthroughKind,
    };
    use rmux_proto::{SessionName, TerminalSize};

    use crate::pane_io::{pane_output_channel, PaneAlertCallback};
    use crate::pane_transcript::PaneTranscript;

    #[test]
    fn osc52_clipboard_set_requires_write_payload() {
        assert!(osc52_payload_is_clipboard_set(b"\x1b]52;c;aGk=\x07"));
        assert!(osc52_payload_is_clipboard_set(b"\x1b]52;c;aGk\x1b\\"));
        assert!(!osc52_payload_is_clipboard_set(b"\x1b]52;c;?\x07"));
        assert!(!osc52_payload_is_clipboard_set(b"\x1b]52;c;%%\x07"));
        assert!(!osc52_payload_is_clipboard_set(b"\x1b]52;c;abcde\x07"));
        // The classifier must reject the same payloads the write decoder
        // rejects, otherwise the PaneSetClipboard hook fires without a stored
        // buffer (diverges from the frozen tmux 3.7b oracle: paste_add and
        // notify_pane must fire together or not at all).
        assert!(!osc52_payload_is_clipboard_set(b"\x1b]52;c;\x07"));
        assert!(!osc52_payload_is_clipboard_set(b"\x1b]52;c;==\x07"));
        assert!(!osc52_payload_is_clipboard_set(b"\x1b]52;c;!!!\x07"));
    }

    #[test]
    fn decode_base64_standard_handles_padding_and_missing_padding() {
        assert_eq!(
            crate::clipboard_protocol::decode_pane_clipboard_write_payload(b"aGVsbG8=").as_deref(),
            Some(&b"hello"[..])
        );
        // OSC 52 producers sometimes omit the trailing padding.
        assert_eq!(
            crate::clipboard_protocol::decode_pane_clipboard_write_payload(b"aGVsbG8").as_deref(),
            Some(&b"hello"[..])
        );
        assert_eq!(
            crate::clipboard_protocol::decode_pane_clipboard_write_payload(b"aGk=").as_deref(),
            Some(&b"hi"[..])
        );
        assert_eq!(
            crate::clipboard_protocol::decode_pane_clipboard_write_payload(b""),
            None
        );
        // Invalid symbols / lengths are rejected rather than decoded to garbage.
        assert_eq!(
            crate::clipboard_protocol::decode_pane_clipboard_write_payload(b"%%"),
            None
        );
        assert_eq!(
            crate::clipboard_protocol::decode_pane_clipboard_write_payload(b"abcde"),
            None
        );
    }

    #[test]
    fn osc52_clipboard_write_payload_decodes_writes_and_skips_queries() {
        assert_eq!(
            osc52_clipboard_write_payload(&TerminalPassthrough::clipboard(
                b"\x1b]52;c;aGVsbG8=\x07".to_vec()
            ))
            .as_deref(),
            Some(&b"hello"[..])
        );
        // The ST terminator form decodes identically.
        assert_eq!(
            osc52_clipboard_write_payload(&TerminalPassthrough::clipboard(
                b"\x1b]52;c;aGk\x1b\\".to_vec()
            ))
            .as_deref(),
            Some(&b"hi"[..])
        );
        // A query carries no write payload.
        assert!(
            osc52_clipboard_write_payload(&TerminalPassthrough::clipboard(
                b"\x1b]52;c;?\x07".to_vec()
            ))
            .is_none()
        );
        // A non-clipboard passthrough is ignored.
        assert!(osc52_clipboard_write_payload(&TerminalPassthrough::raw(
            0,
            0,
            b"\x1b]52;c;aGk=\x07".to_vec()
        ))
        .is_none());
    }

    #[test]
    fn osc52_empty_payload_is_dropped_matching_oracle() {
        // tmux 3.7b's input_osc_52 returns early on an empty payload — no
        // paste_add, no outer forward. rmux must not create an empty buffer.
        assert!(
            osc52_clipboard_write_payload(&TerminalPassthrough::clipboard(
                b"\x1b]52;c;\x07".to_vec()
            ))
            .is_none()
        );
        // Neither should a base64 payload that decodes to zero bytes (a lone
        // padding sequence).
        assert!(
            osc52_clipboard_write_payload(&TerminalPassthrough::clipboard(
                b"\x1b]52;c;==\x07".to_vec()
            ))
            .is_none()
        );
    }

    #[test]
    fn osc52_invalid_base64_payload_is_dropped_matching_oracle() {
        assert!(
            osc52_clipboard_write_payload(&TerminalPassthrough::clipboard(
                b"\x1b]52;c;!!!\x07".to_vec()
            ))
            .is_none()
        );
        assert!(
            osc52_clipboard_write_payload(&TerminalPassthrough::clipboard(
                b"\x1b]52;c;@@@@\x07".to_vec()
            ))
            .is_none()
        );
    }

    #[test]
    fn title_alert_callback_runs_inside_the_transcript_publication_boundary() {
        let transcript = PaneTranscript::shared(2_000, TerminalSize { cols: 80, rows: 24 });
        let callback_transcript = Arc::clone(&transcript);
        let callback_observed = Arc::new(AtomicBool::new(false));
        let callback_observed_clone = Arc::clone(&callback_observed);
        let callback: PaneAlertCallback = Arc::new(move |event| {
            assert!(event.title_change.is_some(), "OSC 2 must change the title");
            assert!(
                callback_transcript.try_lock().is_err(),
                "the title callback must run before the transcript publication lock is released"
            );
            callback_observed_clone.store(true, Ordering::Release);
        });
        let output = pane_output_channel();
        let session_name = SessionName::new("title-linearization").expect("valid session name");

        let _ = publish_pane_bytes(
            PanePublishContext {
                session_name: &session_name,
                pane_id: PaneId::new(1),
                transcript: &transcript,
                pane_output: &output,
                generation: None,
                pane_alert_callback: Some(&callback),
                emit_no_bell_alert: false,
            },
            b"\x1b]2;linearized-title\x07".to_vec(),
        );

        assert!(callback_observed.load(Ordering::Acquire));
    }

    #[test]
    fn mouse_mode_alert_is_stamped_by_production_pane_publication() {
        let transcript = PaneTranscript::shared(2_000, TerminalSize { cols: 80, rows: 24 });
        let callback_transcript = Arc::clone(&transcript);
        let callback_observed = Arc::new(AtomicBool::new(false));
        let callback_observed_clone = Arc::clone(&callback_observed);
        let callback: PaneAlertCallback = Arc::new(move |event| {
            assert!(
                event.mouse_mode_changed,
                "the reader publication path must stamp a real mouse-mode transition"
            );
            assert!(
                callback_transcript.try_lock().is_err(),
                "mouse-mode comparison and callback must share the transcript boundary"
            );
            callback_observed_clone.store(true, Ordering::Release);
        });
        let output = pane_output_channel();
        let session_name = SessionName::new("mouse-mode-stamping").expect("valid session name");

        let _ = publish_pane_bytes(
            PanePublishContext {
                session_name: &session_name,
                pane_id: PaneId::new(1),
                transcript: &transcript,
                pane_output: &output,
                generation: None,
                pane_alert_callback: Some(&callback),
                emit_no_bell_alert: false,
            },
            b"\x1b[?1003h".to_vec(),
        );

        assert!(callback_observed.load(Ordering::Acquire));
    }

    #[test]
    fn production_publication_invalidates_recovery_at_the_post_rep_boundary() {
        let transcript = PaneTranscript::shared(2_000, TerminalSize { cols: 8, rows: 2 });
        let output = pane_output_channel();
        let mut recovery = output.subscribe();
        let mut legacy = output.subscribe();
        let session_name = SessionName::new("rep-recovery").expect("valid session name");
        let context = || PanePublishContext {
            session_name: &session_name,
            pane_id: PaneId::new(1),
            transcript: &transcript,
            pane_output: &output,
            generation: None,
            pane_alert_callback: None,
            emit_no_bell_alert: false,
        };

        let _ = publish_pane_bytes(context(), b"X".to_vec());
        assert!(matches!(
            recovery.try_recv_observed(),
            Some(crate::pane_io::PaneObservationItem::Output(
                rmux_core::events::OutputCursorItem::Event(event)
            )) if event.bytes() == b"X"
        ));
        let _ = legacy.try_recv().expect("legacy subscriber receives X");

        let _ = publish_pane_bytes(context(), b"\x1b[2b".to_vec());

        let Some(crate::pane_io::PaneObservationItem::Invalidated(invalidation)) =
            recovery.try_recv_observed()
        else {
            panic!("recoverable subscriber must skip REP and rebase");
        };
        assert_eq!(
            invalidation.reason,
            crate::pane_io::PaneInvalidationReason::TranscriptMutation
        );
        assert_eq!(invalidation.boundary.next_output_sequence, 2);
        assert!(
            recovery.try_recv_observed().is_none(),
            "the non-replayable REP event must not follow its invalidation"
        );

        let rmux_core::events::OutputCursorItem::Event(event) =
            legacy.try_recv().expect("legacy attach still receives REP")
        else {
            panic!("legacy REP must remain an output event");
        };
        assert_eq!(event.bytes(), b"\x1b[2b");
    }

    #[test]
    fn clipboard_query_is_typed_by_production_pane_publication() {
        let transcript = PaneTranscript::shared(2_000, TerminalSize { cols: 80, rows: 24 });
        let callback_observed = Arc::new(AtomicBool::new(false));
        let callback_observed_clone = Arc::clone(&callback_observed);
        let callback: PaneAlertCallback = Arc::new(move |event| {
            assert!(
                !event.clipboard_set,
                "a query must not be classified as a write"
            );
            assert!(event.clipboard_writes.is_empty());
            assert_eq!(
                event.clipboard_queries,
                vec![TerminalClipboardQuery::new("zzpc", InputEndType::St)]
            );
            callback_observed_clone.store(true, Ordering::Release);
        });
        let output = pane_output_channel();
        let mut output_rx = output.subscribe();
        let session_name =
            SessionName::new("clipboard-query-stamping").expect("valid session name");

        let _ = publish_pane_bytes(
            PanePublishContext {
                session_name: &session_name,
                pane_id: PaneId::new(1),
                transcript: &transcript,
                pane_output: &output,
                generation: None,
                pane_alert_callback: Some(&callback),
                emit_no_bell_alert: false,
            },
            b"\x1b]52;zzpc;?\x1b\\".to_vec(),
        );

        assert!(callback_observed.load(Ordering::Acquire));
        let frame = output_rx
            .try_recv()
            .expect("published pane frame remains observable");
        let rmux_core::events::OutputCursorItem::Event(frame) = frame else {
            panic!("published pane frame must be an event");
        };
        assert_eq!(frame.passthroughs().len(), 1);
        assert_eq!(
            frame.passthroughs()[0].kind(),
            TerminalPassthroughKind::Clipboard
        );
        assert!(
            frame.passthroughs()[0].render_sequence().is_empty(),
            "the generic passthrough renderer must never leak a query outward"
        );
    }
}
