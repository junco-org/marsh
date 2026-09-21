use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const EXIT_LOG_ENV: &str = "RMUX_ATTACH_EXIT_LOG";

pub(crate) fn record_shutdown_request(reason: &str) {
    record_line(&format!(
        "time_ms={} process_pid={} event=shutdown-request reason={reason}",
        timestamp_ms(),
        std::process::id()
    ));
}

pub(crate) fn record_shutdown_queued(reason: &str) {
    record_line(&format!(
        "time_ms={} process_pid={} event=shutdown-queued reason={reason}",
        timestamp_ms(),
        std::process::id()
    ));
}

/// Records a shell job's output stream failing for a reason that is not end of file.
///
/// A stream ending badly is a diagnostic about this daemon's own plumbing, not a statement about
/// the program: the job is still running, and rendering this as a pane exit would invent a status
/// nothing produced.
pub(crate) fn record_shell_stream_error(shell: &str, uid: &str, channel: &str, error: &str) {
    record_line(&format!(
        "time_ms={} process_pid={} event=shell-stream-error shell={shell} uid={uid} \
         channel={channel} error={error}",
        timestamp_ms(),
        std::process::id()
    ));
}

/// Records a command whose publication the gate refused.
///
/// Only unapproved verdicts are written. A published line is the ordinary case and says nothing an
/// operator reading this log is looking for; a denial, a stale snapshot, a discard or an
/// infrastructure failure is why a workload that exited zero changed nothing.
pub(crate) fn record_shell_command_unapproved(shell: &str, uid: &str, command: &str, verdict: &str) {
    record_line(&format!(
        "time_ms={} process_pid={} event=shell-command-unapproved shell={shell} uid={uid} \
         verdict={verdict} command={command}",
        timestamp_ms(),
        std::process::id()
    ));
}

/// Records requested work this daemon never ran, or ran without ever reaching a verdict.
///
/// [`record_shell_command_unapproved`] answers "the workload ran and the gate refused it". This
/// answers the other half: the workload was admitted nowhere, was rejected before it started, or
/// its collection failed, so no completion exists for the verdict log to describe. Both halves
/// leave the seed unchanged, and a caller with no error channel — a hook entry, by construction —
/// otherwise leaves no trace of either.
///
/// `kind` names the requesting path, `reason` is free text and comes last so an error message
/// containing separators cannot swallow a later field.
pub(crate) fn record_workload_not_run(kind: &str, command: &str, reason: &str) {
    record_line(&format!(
        "time_ms={} process_pid={} event=workload-not-run kind={kind} command='{command}' \
         reason={reason}",
        timestamp_ms(),
        std::process::id()
    ));
}

fn record_line(line: &str) {
    let Some(path) = std::env::var_os(EXIT_LOG_ENV) else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

fn timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}
