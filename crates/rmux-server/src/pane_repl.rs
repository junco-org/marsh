//! The interactive driver behind a pane with no command: the thing that actually reads a
//! keyboard and runs what was typed.
//!
//! A pane opened without a command is an *idle terminal job*. The shell engine will happily run
//! lines in it, but it will not invent them: nothing reads the terminal, nothing composes a
//! prompt, and nothing decides when a typed line is finished. Without this module a user attaches
//! to a pane, types, and watches their keystrokes accumulate on a slave nobody is reading.
//!
//! So each commandless pane gets one driver task. It leases the pane's idle terminal, renders a
//! prompt, edits a line with the same editor rmux's own command prompt uses, and — once the line
//! is complete — releases the lease and submits it.
//!
//! # Everything it renders goes through the lease
//!
//! A lease is the *slave* side of the pane's pseudoterminal, which is the side a program writes
//! its output on. That is the only correct surface for a prompt, an echo or a verdict: it places
//! them after output already queued in the same terminal, even when a command's verdict outruns
//! the master pump.
//!
//! Writing to the job's standard input instead would be a category error with a visible symptom:
//! input flows master to slave, so the driver's own error text would come back to it as
//! keystrokes, and the next prompt would read its own diagnostics as if the user had typed them.
//!
//! # Why the lease is given back before anything runs
//!
//! `brush-interactive` reads the *process's* standard input, which is exactly wrong for a daemon
//! holding many panes: there is one stdin and there are many prompts. A lease is this pane's own
//! terminal, held only while no command is running in it, and revoked the instant one is admitted.
//!
//! Releasing it before *any* mutating call into the multiplexer is what keeps this task from
//! waiting on itself. `start_in`, a stop and an end-of-input closure all wait for the outstanding
//! lease to be acknowledged before they proceed; holding the lease across one of them is a
//! deadlock, not a slow path.
//!
//! # Not a second editor
//!
//! The line editing here is [`PromptBuffer`], the same text/cursor/kill/history state rmux's
//! command prompt edits, and the keys are decoded by the same decoder an attached client's input
//! goes through. What belongs to this prompt alone is what genuinely differs from a status-line
//! prompt: a pseudoterminal to paint on, a shell to ask whether a line is finished, and the mux
//! grammar a submitted line is resolved against.

use std::io::Write as _;
use std::time::Duration;

use marsh_core::shellmux::{
    CommandOptions, IdleTerminal, JobIo, JobView, MuxError, ShellId, jobctl, repl,
};
use rmux_core::{Utf8Config, text_width};

use crate::handler::pane_support::pane_prompt_input::decode_prompt_input_event;
use crate::handler::prompt_support::PromptInputEvent;
use crate::io::{IoError, ShellHandle, ShellIo};
use crate::prompt_buffer::{self, PromptBuffer};

/// How many bytes one read takes at most.
const CHUNK: usize = 4096;

/// How long a lone Escape is held before it is taken for the Escape key, when the `escape-time`
/// option cannot be read because no request handler is bound.
///
/// The option's own default, so a prompt on an unbound facade behaves like one on a bound facade
/// that was never reconfigured.
const DEFAULT_ESCAPE_TIME: Duration = Duration::from_millis(10);

/// How many submitted lines one pane's prompt remembers.
const HISTORY_LIMIT: usize = 100;

/// Where a word kill stops, beyond whitespace.
///
/// A path separator and the shell's own operators, so a second Ctrl-W over `src/lib.rs` takes the
/// file and then the directory rather than the whole argument at once.
const WORD_SEPARATORS: &str = "/\\|&;<>()";

/// Turns the prompt's bracketed paste mode on, so a paste arrives wrapped rather than as keys.
const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
/// Turns it back off, which is the state a terminal is in before a program asks otherwise.
const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";
/// What wraps the front of a bracketed paste.
const PASTE_START: &[u8] = b"\x1b[200~";
/// What wraps the end of one.
const PASTE_END: &[u8] = b"\x1b[201~";

/// Starts the driver for one commandless pane.
///
/// One task per pane, owned by the daemon's runtime. It ends when the pane's job closes, which is
/// the only condition under which a pane stops needing a prompt.
pub(crate) fn spawn(io: ShellIo, job: ShellHandle) {
    let runtime = io.runtime();
    runtime.spawn(async move {
        Prompt::new(io, job).drive().await;
    });
}

/// One pane's prompt, across every lease it takes.
///
/// The state that survives a revocation lives here rather than in the editing loop: a half-typed
/// line, an unfinished multi-line construct, undecoded bytes of a split escape sequence, and a
/// verdict that has not been drawn yet all outlive the lease they were produced under.
struct Prompt {
    /// The facade this pane's job belongs to.
    io: ShellIo,
    /// This pane's job, generation-bound.
    job: ShellHandle,
    /// What is being typed, and what has arrived but not decoded yet.
    line: Editing,
    /// What the last command left to say, owed to the terminal before the next prompt.
    report: Vec<String>,
    /// Which command the pending report belongs to, so the job's close does not render it again.
    ///
    /// Marked only once every line has actually reached the slave. A verdict marked before the
    /// write succeeded would be suppressed at close having never been shown at all, which is worse
    /// than showing it twice.
    report_command: Option<marsh_core::shellmux::CommandId>,
    /// How many rows below its first the last painted line occupied.
    rendered_rows: usize,
    /// Whether this prompt has advertised bracketed paste mode on the terminal it is editing on.
    ///
    /// The state to put back is the state the terminal was in before the prompt asked, and for a
    /// prompt that is *off*: a program that turns the mode on owns it only while it runs, and the
    /// prompt restores before it hands the terminal over, so the mode is never on when a session
    /// begins.
    bracketed: bool,
}

/// The line being typed, and the bytes that have not become part of it yet.
///
/// Deliberately separate from the terminal half: everything here is a pure function of the bytes
/// that arrived, so what a keystroke, a split character or a pasted newline does to a line is
/// decided — and tested — without a pseudoterminal, a multiplexer or a seed.
#[derive(Default)]
struct Editing {
    /// The line being edited: text, caret, kill slot and history cursor.
    editor: PromptBuffer,
    /// Lines this prompt has submitted, oldest first.
    history: Vec<String>,
    /// The already-accepted lines of an unfinished construct, each ending in its own newline.
    ///
    /// The *original* edit buffer, never the parser's trimmed command text: a line whose quote is
    /// still open owns its trailing spaces, and re-submitting a trimmed copy would run a different
    /// command from the one that was typed.
    continuation: String,
    /// Bytes read from the terminal that have not decoded into an event yet.
    ///
    /// A lease may split a UTF-8 character or an escape sequence across two reads, so a partial
    /// tail is retained rather than decoded into whatever it looks like so far.
    pending: Vec<u8>,
    /// The body of a bracketed paste being collected, between its two markers.
    ///
    /// Collected whole and inserted once. A paste is one edit: its newlines go into the line
    /// rather than submitting it, and decoding it in arriving chunks would corrupt a character
    /// that straddles two reads.
    paste: Option<Vec<u8>>,
}

/// How one editing session ended.
enum Session {
    /// A complete line was submitted; what it parsed to, and whatever was typed after its Enter.
    Submitted {
        /// The parsed line, resolved before anything ran.
        input: repl::Input,
        /// Raw bytes that arrived in the same read, after the Enter.
        suffix: Vec<u8>,
    },
    /// The lease was revoked because a command is starting: the half-typed line is kept.
    Revoked,
    /// End of input on an empty line: this shell closes.
    Eof,
    /// The terminal is gone.
    Closed,
}

/// What one decoded event did to the line.
enum Step {
    /// Nothing worth repainting.
    Idle,
    /// The line changed.
    Changed,
    /// Enter: the line is finished being typed.
    Submit,
    /// Ctrl-C: abandon what is being typed and prompt again.
    Cancel,
    /// Ctrl-D on an empty line.
    Eof,
}

/// Something the terminal half owes after a run of keys was decoded.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Ctrl-C: abandon what is being typed and prompt again.
    Cancel,
    /// Enter: the line is finished being typed.
    Submit,
    /// Ctrl-D on an empty line.
    Eof,
}

/// What decoding the retained input decided.
#[derive(Debug, PartialEq, Eq)]
struct Advance {
    /// Whether the painted line no longer matches the line being edited.
    redraw: bool,
    /// What stopped the decoding, when something did.
    ///
    /// Decoding stops *at* an action with the rest of the input still retained, so the bytes
    /// typed after an Enter are still there to be claimed by whatever that Enter starts.
    action: Option<Action>,
}

impl Editing {
    /// Decodes as much of the retained input as it can, applying it to the line.
    ///
    /// Stops at the first key the terminal half has to act on, and at the first byte that cannot
    /// be decoded yet: a partial UTF-8 character, an unfinished escape sequence, half a paste
    /// marker, or a lone Escape whose meaning `escape-time` has not settled.
    fn advance(&mut self) -> Advance {
        let mut redraw = false;
        while !self.pending.is_empty() {
            if self.paste.is_some() {
                if !self.collect_paste() {
                    break;
                }
                redraw = true;
                continue;
            }

            if self.pending.starts_with(PASTE_START) {
                self.pending.drain(..PASTE_START.len());
                self.paste = Some(Vec::new());
                continue;
            }
            // A paste marker split across two reads is not an escape sequence to interpret, and
            // an Escape prefix is not the Escape key until `escape-time` says so. Both stay
            // where they are; the terminal half decides when the second one has waited enough.
            if is_partial(&self.pending, PASTE_START) || escape_undecided(&self.pending) {
                break;
            }

            let Some((event, consumed)) = decode_prompt_input_event(&self.pending) else {
                break;
            };
            let action = match self.apply(&event) {
                Step::Idle => None,
                Step::Changed => {
                    redraw = true;
                    None
                }
                Step::Cancel => Some(Action::Cancel),
                Step::Submit => Some(Action::Submit),
                Step::Eof => Some(Action::Eof),
            };
            self.pending.drain(..consumed);
            if action.is_some() {
                return Advance { redraw, action };
            }
        }

        Advance {
            redraw,
            action: None,
        }
    }

    /// Applies one decoded key to the line.
    fn apply(&mut self, event: &PromptInputEvent) -> Step {
        match event {
            PromptInputEvent::Char(ch) => {
                self.editor.push_char(*ch);
                Step::Changed
            }
            PromptInputEvent::Enter => Step::Submit,
            PromptInputEvent::Backspace => changed(self.editor.delete_left()),
            PromptInputEvent::Delete => changed(self.editor.delete_at_cursor()),
            PromptInputEvent::Left | PromptInputEvent::Ctrl('b') => changed(self.editor.move_left()),
            PromptInputEvent::Right | PromptInputEvent::Ctrl('f') => {
                changed(self.editor.move_right())
            }
            PromptInputEvent::Home | PromptInputEvent::Ctrl('a') => changed(self.editor.move_home()),
            PromptInputEvent::End | PromptInputEvent::Ctrl('e') => changed(self.editor.move_end()),
            PromptInputEvent::Up | PromptInputEvent::Ctrl('p') => {
                changed(self.editor.history_up(&self.history))
            }
            PromptInputEvent::Down | PromptInputEvent::Ctrl('n') => {
                changed(self.editor.history_down(&self.history))
            }
            PromptInputEvent::Ctrl('u') => changed(self.editor.clear_buffer()),
            PromptInputEvent::Ctrl('k') => changed(self.editor.delete_to_end()),
            PromptInputEvent::Ctrl('w') => changed(self.editor.delete_word_left(WORD_SEPARATORS)),
            PromptInputEvent::Ctrl('y') => changed(self.editor.paste_saved()),
            PromptInputEvent::Ctrl('c') => Step::Cancel,
            // End of input, but only where a shell would take it as one: on an empty line, with
            // no unfinished construct above it. Anywhere else Ctrl-D is the forward delete it is
            // in every line editor, and closing the pane instead would lose the line.
            PromptInputEvent::Ctrl('d') => {
                if self.editor.is_empty() && self.continuation.is_empty() {
                    Step::Eof
                } else {
                    changed(self.editor.delete_at_cursor())
                }
            }
            // Escape is resolved rather than acted on, there is no completion to offer, and an
            // unbound control or named key is not an editing operation.
            PromptInputEvent::Escape
            | PromptInputEvent::Tab
            | PromptInputEvent::Ctrl(_)
            | PromptInputEvent::KeyName(_) => Step::Idle,
        }
    }

    /// Collects the bracketed paste in flight, answering whether it finished.
    ///
    /// The body is taken verbatim. Its newlines are line breaks *in the line being edited*, which
    /// is the whole point of advertising the mode: a paste that arrived as bare keys would submit
    /// at its first newline and run half of itself.
    fn collect_paste(&mut self) -> bool {
        let Some(body) = self.paste.as_mut() else {
            return false;
        };
        match find(&self.pending, PASTE_END) {
            Some(at) => {
                body.extend_from_slice(&self.pending[..at]);
                self.pending.drain(..at + PASTE_END.len());
                let text = String::from_utf8_lossy(body)
                    .replace("\r\n", "\n")
                    .replace('\r', "\n");
                self.paste = None;
                self.editor.insert_text(&text);
                true
            }
            None => {
                // The end marker may be split across this read and the next, so whatever could
                // still become one stays where it is.
                let keep = trailing_partial(&self.pending, PASTE_END);
                let take = self.pending.len() - keep;
                body.extend_from_slice(&self.pending[..take]);
                self.pending.drain(..take);
                false
            }
        }
    }

    /// Folds the finished line into the construct above it, and starts a new one.
    ///
    /// Answers the whole submitted text — continuation lines included, verbatim — and leaves the
    /// editor empty.
    fn take_submitted(&mut self) -> String {
        let mut full = std::mem::take(&mut self.continuation);
        full.push_str(self.editor.text());
        self.editor.clear();
        full
    }

    /// Keeps `full` as an unfinished construct, so the next line continues it.
    fn continue_with(&mut self, mut full: String) {
        full.push('\n');
        self.continuation = full;
    }

    /// Abandons the line and the construct above it.
    fn cancel(&mut self) {
        self.continuation.clear();
        self.editor.clear();
    }
}

impl Prompt {
    /// A prompt with nothing typed, for `job`.
    fn new(io: ShellIo, job: ShellHandle) -> Self {
        Self {
            io,
            job,
            line: Editing::default(),
            report: Vec::new(),
            report_command: None,
            rendered_rows: 0,
            bracketed: false,
        }
    }

    /// Reads, edits and runs lines until the pane's job closes.
    async fn drive(mut self) {
        loop {
            let Some(lease) = self.acquire().await else {
                return;
            };

            let session = self.session(&lease).await;
            // Released before *anything* mutating: `start_in`, a stop and an end-of-input closure
            // all wait for this lease to be acknowledged first.
            self.release(lease).await;

            match session {
                Session::Submitted { input, suffix } => {
                    if self.run(input, suffix).await.is_break() {
                        return;
                    }
                }
                Session::Revoked => {}
                Session::Eof => {
                    let _ = self.io.stop(&self.job, false).await;
                    return;
                }
                Session::Closed => return,
            }
        }
    }

    /// Takes the pane's idle-terminal lease, waiting out whatever is using it.
    ///
    /// `None` when the job has closed or the host is shutting down — the two conditions under
    /// which a pane stops needing a prompt at all. A busy terminal is neither: a command is
    /// running in the pane, and this waits for it rather than giving up on the pane.
    async fn acquire(&self) -> Option<IdleTerminal> {
        loop {
            match self.io.idle_terminal(&self.job).await {
                Ok(lease) => return Some(lease),
                Err(IoError::Closed) => return None,
                Err(IoError::Mux(error)) => match &*error {
                    MuxError::TerminalBusy(_) | MuxError::JobNotReady(_) => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    _ => return None,
                },
                Err(_) => return None,
            }
        }
    }

    /// Puts the terminal's paste mode back and gives the lease up.
    ///
    /// The restore goes out *before* the drop, because dropping is what acknowledges the release —
    /// and a command must not start into a terminal still advertising the prompt's paste mode. A
    /// close-revocation refuses the write, which is correct: that surface is going away, so there
    /// is nothing left to restore it for.
    async fn release(&mut self, lease: IdleTerminal) {
        if self.bracketed {
            let _ = lease.write_all(DISABLE_BRACKETED_PASTE).await;
            self.bracketed = false;
        }
        drop(lease);
    }

    /// Renders a prompt and edits until the line is submitted or the terminal is taken away.
    async fn session(&mut self, lease: &IdleTerminal) -> Session {
        // Advertised on the slave, so it reaches this pane's own screen state and nothing else.
        // A process-global terminal toggle here would change the paste behaviour of every other
        // pane in the daemon.
        if lease.write_all(ENABLE_BRACKETED_PASTE).await.is_err() {
            return Session::Closed;
        }
        self.bracketed = true;

        let pending = std::mem::take(&mut self.report);
        let rendered = self.report_command.take();
        for line in pending {
            if lease
                .write_all(format!("{line}\r\n").as_bytes())
                .await
                .is_err()
            {
                // Nothing is marked: the verdict never reached the slave, so the job's close must
                // still render it. Marking here would suppress a report the user never saw.
                return Session::Closed;
            }
        }
        // Only now, with every line on the slave. The job's close renders a verdict the prompt did
        // not manage to show, and suppresses one it did.
        if let Some(command) = rendered {
            self.io
                .mark_report_rendered(&self.job.sandbox().uid, command);
        }

        if self.paint(lease, true).await.is_err() {
            return Session::Closed;
        }

        let mut buffer = [0u8; CHUNK];
        loop {
            if let Some(session) = self.consume(lease).await {
                return session;
            }

            // An Escape that has nothing after it yet, or only the `[` that opens every CSI, is
            // the one ambiguity a longer read cannot settle by itself: it is either the Escape
            // key or the head of a sequence still arriving. `escape-time` is the option that
            // decides how long to wait for the rest, exactly as it does for an attached client's
            // keyboard. Every longer prefix is retained until it completes, because a paste or a
            // terminal reply legitimately pauses far longer than a keystroke does.
            let read = if escape_undecided(&self.line.pending) {
                match tokio::time::timeout(self.escape_time().await, lease.read(&mut buffer)).await
                {
                    Ok(read) => read,
                    Err(_elapsed) => {
                        // Resolved as the Escape key, which this prompt does nothing with, and
                        // whatever followed it is then itself. What matters is that neither keeps
                        // blocking the bytes behind them.
                        self.line.pending.remove(0);
                        continue;
                    }
                }
            } else {
                lease.read(&mut buffer).await
            };

            match read {
                Ok(None) => return Session::Revoked,
                Ok(Some(0)) => return Session::Closed,
                Ok(Some(count)) => self.line.pending.extend_from_slice(&buffer[..count]),
                Err(_) => return Session::Closed,
            }
        }
    }

    /// Applies everything that has arrived, and draws what it did.
    ///
    /// `None` means the line is still being typed and more bytes are needed.
    async fn consume(&mut self, lease: &IdleTerminal) -> Option<Session> {
        loop {
            let step = self.line.advance();
            // The echo is owed *before* the key that ended the run is acted on: a burst holding
            // a whole line and its Enter must still show the line it submitted.
            if step.redraw && self.paint(lease, false).await.is_err() {
                return Some(Session::Closed);
            }
            match step.action {
                None => return None,
                Some(Action::Cancel) => {
                    self.line.cancel();
                    if lease.write_all(b"^C\r\n").await.is_err() {
                        return Some(Session::Closed);
                    }
                    self.rendered_rows = 0;
                    if self.paint(lease, true).await.is_err() {
                        return Some(Session::Closed);
                    }
                }
                Some(Action::Eof) => {
                    let _ = lease.write_all(b"\r\n").await;
                    return Some(Session::Eof);
                }
                Some(Action::Submit) => match self.submit_line(lease).await {
                    Submission::Session(session) => return Some(session),
                    Submission::Reprompt => {}
                },
            }
        }
    }

    /// Decides what a finished line means: run it, or keep taking more of it.
    async fn submit_line(&mut self, lease: &IdleTerminal) -> Submission {
        if lease.write_all(b"\r\n").await.is_err() {
            return Submission::Session(Session::Closed);
        }
        self.rendered_rows = 0;

        let full = self.line.take_submitted();
        let input = repl::parse(&full);
        // Only a real command line can be unfinished. A frontend builtin has its own grammar, and
        // asking brush whether `exit` is complete would answer about a different `exit`.
        let command = match &input {
            repl::Input::Foreground(cmd) | repl::Input::Background { cmd, .. } => Some(cmd.clone()),
            _ => None,
        };
        if let Some(command) = command {
            if matches!(
                self.io.input_is_complete(&self.job, &command).await,
                Ok(false)
            ) {
                // The verbatim buffer, newline included: a construct that lost its line breaks —
                // or the spaces inside its open quote — would be a different command.
                self.line.continue_with(full);
                if self.paint(lease, true).await.is_err() {
                    return Submission::Session(Session::Closed);
                }
                return Submission::Reprompt;
            }
        }

        if matches!(input, repl::Input::Empty) {
            // Nothing was typed. Prompt again without admitting a job.
            if self.paint(lease, true).await.is_err() {
                return Submission::Session(Session::Closed);
            }
            return Submission::Reprompt;
        }

        prompt_buffer::history_push(&mut self.line.history, full.trim(), HISTORY_LIMIT);
        Submission::Session(Session::Submitted {
            input,
            // Anything typed after the Enter is not this prompt's any more.
            suffix: std::mem::take(&mut self.line.pending),
        })
    }

    /// Paints the prompt and the line being edited, and puts the caret where it belongs.
    ///
    /// `fresh` starts a new block at the current cursor position; otherwise the previously painted
    /// block is erased first. Everything goes out in one write, so a repaint is one atomic chunk
    /// in the terminal stream rather than a visible erase followed by a redraw.
    async fn paint(&mut self, lease: &IdleTerminal, fresh: bool) -> std::io::Result<()> {
        let utf8 = Utf8Config::default();
        let prompt = self.prompt_text();
        let text = self.line.editor.text();
        let rows = text.matches('\n').count();

        let mut out = Vec::with_capacity(prompt.len() + text.len() + 16);
        if !fresh {
            out.push(b'\r');
            if self.rendered_rows > 0 {
                let _ = write!(out, "\x1b[{}A", self.rendered_rows);
            }
            out.extend_from_slice(b"\x1b[J");
        }
        out.extend_from_slice(prompt.as_bytes());
        for (index, segment) in text.split('\n').enumerate() {
            if index > 0 {
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(segment.as_bytes());
        }

        let (row, column) = caret(&prompt, text, self.line.editor.cursor, &utf8);
        if rows > row {
            let _ = write!(out, "\x1b[{}A", rows - row);
        }
        out.push(b'\r');
        if column > 0 {
            let _ = write!(out, "\x1b[{column}C");
        }

        self.rendered_rows = rows;
        lease.write_all(&out).await
    }

    /// What this prompt writes before the line.
    fn prompt_text(&self) -> String {
        if self.line.continuation.is_empty() {
            format!(
                "{} {}> ",
                self.job.id().reference(),
                jobctl::dir_label(self.job.sandbox())
            )
        } else {
            "> ".to_owned()
        }
    }

    /// How long a lone Escape waits for the rest of its sequence.
    async fn escape_time(&self) -> Duration {
        match self.io.handler() {
            Some(handler) => handler.attached_escape_time().await,
            None => DEFAULT_ESCAPE_TIME,
        }
    }

    /// Runs whatever the submitted line turned out to be.
    ///
    /// No lease is held here: the command owns the terminal, its mode and its size while it runs.
    /// Whatever there is to report is kept for the next acquisition, which is what puts it on the
    /// slave after the command's own output rather than racing it.
    ///
    /// `Break` ends the driver: this pane's shell is going away.
    async fn run(&mut self, input: repl::Input, suffix: Vec<u8>) -> std::ops::ControlFlow<()> {
        let report = match input {
            // Resolved in `submit_line`; reaching here would mean a job for nothing.
            repl::Input::Empty => Vec::new(),
            repl::Input::Invalid(message) => vec![message],
            repl::Input::Jobs => self.jobs_table(),
            repl::Input::Exit => {
                // Graceful: whatever is still running concludes and is gated, rather than being
                // discarded because the console went away.
                let _ = self.io.stop(&self.job, false).await;
                return std::ops::ControlFlow::Break(());
            }
            repl::Input::Fg(name) => self.foreground(name.as_deref()).await,
            repl::Input::Stop(args) => self.stop_job(&args).await,
            // Never `jobctl::kill`: a signal asked for as a workload command is workload, and
            // sending it from the frontend would skip admission, instrumentation and the gate
            // that every other line goes through.
            repl::Input::Kill(args) => return self.submit(kill_line(&args), suffix).await,
            repl::Input::SpawnDir { name, dir } => self.spawn_job(name, Some(&dir), None).await,
            repl::Input::Background { cmd, name } => {
                self.spawn_job(name, None, Some(&cmd)).await
            }
            repl::Input::Foreground(cmd) => return self.submit(cmd, suffix).await,
        };

        // Nothing was admitted, so the bytes typed after the Enter are still this prompt's.
        self.line.pending = suffix;
        self.report = report;
        std::ops::ControlFlow::Continue(())
    }

    /// Submits one command line into this pane's own job and waits for its verdict.
    async fn submit(&mut self, cmd: String, suffix: Vec<u8>) -> std::ops::ControlFlow<()> {
        let command = match self
            .io
            .start_in(&self.job, &cmd, CommandOptions::default())
            .await
        {
            Ok(command) => command,
            Err(error) => {
                // Nothing runs, so the typeahead belongs to the next prompt rather than to a
                // command that was never admitted.
                self.line.pending = suffix;
                self.report = vec![format!("{}: {error}", self.job.id().reference())];
                return std::ops::ControlFlow::Continue(());
            }
        };

        // The command is reserved and launching, so these bytes cannot be read by anything else:
        // they are this program's standard input if it reads one, and stay in the pseudoterminal
        // for the next prompt if it does not. Written once, after the reservation, never beside
        // it.
        if !suffix.is_empty() {
            if let Err(error) = self.io.write_input(&self.job, &suffix).await {
                tracing::debug!(
                    shell = self.job.id().as_str(),
                    "dropping input typed after a submitted line: {error}"
                );
            }
        }

        let Ok(completion) = command.wait().await else {
            // The host was torn down, or the producer was lost. Either way this pane is not
            // getting a verdict, and inventing one would be worse than saying nothing.
            return std::ops::ControlFlow::Continue(());
        };
        // Every outcome that is not an ordinary publication is reported: a denial, a stale path
        // and a discard are all things the user must see, because the command may have exited zero
        // and still changed nothing.
        let report = repl::report_lines(self.job.id(), &completion.outcome);
        self.deliver(report, Some(completion.id)).await
    }

    /// Puts a verdict where it can still be read.
    ///
    /// An open job gets it on its own terminal, before the next prompt. A job that was already
    /// closing when the verdict arrived has no terminal left to prompt on: reopening one to print
    /// a single line would draw a prompt into a pane that is going away, so the report is left
    /// for the job's own retirement to append through the same transcript publisher every other
    /// byte of that pane went through.
    async fn deliver(
        &mut self,
        report: Vec<String>,
        command: Option<marsh_core::shellmux::CommandId>,
    ) -> std::ops::ControlFlow<()> {
        let closing = self
            .io
            .job(self.job.id())
            .is_none_or(|view: JobView| view.closing);
        if !closing {
            self.report = report;
            self.report_command = command;
            return std::ops::ControlFlow::Continue(());
        }

        // Nothing is deposited here. A closing job's verdict is rendered by the terminal delivery
        // worker, which derives it from the job's own `JobEnd::completion` immediately before it
        // emits the pane's end of file. Depositing from this side made the outcome depend on
        // whether `command.wait()` returned before or after that worker looked — the report
        // appeared, or was silently dropped, according to a race nobody could see.
        drop(report);
        std::ops::ControlFlow::Break(())
    }

    /// The job table, rendered exactly as the console builtin renders it.
    fn jobs_table(&self) -> Vec<String> {
        let mut rendered = Vec::new();
        self.io.print_jobs(&mut rendered);
        String::from_utf8_lossy(&rendered)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Makes another job the selected one.
    async fn foreground(&self, name: Option<&str>) -> Vec<String> {
        let target = match name {
            Some(name) => ShellId::from(name),
            // The most recently created job a user can actually be attached to. A pipe helper is
            // real work with a real principal and no terminal, so selecting one would hand the
            // foreground to something that has no keyboard.
            None => match self
                .io
                .jobs()
                .into_iter()
                .filter(|view| matches!(view.io, JobIo::Terminal { .. }) && !view.closing)
                .next_back()
            {
                Some(view) => view.id,
                None => return vec!["fg: no current job".to_owned()],
            },
        };
        let handle = match self.io.shell(&target) {
            Ok(handle) => handle,
            Err(error) => return vec![format!("fg: {error}")],
        };
        match self.io.switch(&handle).await {
            Ok(view) => vec![format!("{} selected", view.id.reference())],
            Err(error) => vec![format!("fg: {error}")],
        }
    }

    /// Opens a new job in a window of its own, optionally with a command, without waiting for it.
    ///
    /// `dir` is a directory as typed, resolved against where this job's shell currently stands;
    /// `None` opens the new job right there.
    ///
    /// The window is what makes the job usable. A job with a snapshot and a principal and no
    /// surface cannot be seen, selected or typed into, so both forms go through
    /// [`RequestHandler::spawn_repl_window`](crate::handler::RequestHandler::spawn_repl_window):
    /// the same plan/open/commit transaction `new-window` runs, in this job's own session,
    /// detached, carrying the parsed id and the engine's own lifetime rules.
    ///
    /// A prompt holds a facade and a handle, not handler state, so [`ShellIo::handler`] is the
    /// way in. When no handler is bound there is no session to open a window in, and that is
    /// reported rather than papered over: opening the job through the facade alone would answer
    /// `%3 started` for a job with nowhere to appear.
    async fn spawn_job(
        &self,
        name: Option<String>,
        dir: Option<&str>,
        cmd: Option<&str>,
    ) -> Vec<String> {
        if let Some(name) = &name {
            if !repl::valid_name(name) {
                return vec![format!("sd: {name}: not a usable job name")];
            }
        }
        let base = match self.snapshot_relative_cwd() {
            Ok(base) => base,
            Err(message) => return vec![message],
        };
        let dir = match dir {
            Some(dir) => repl::job_dir(&base, dir),
            None => base,
        };
        let Some(handler) = self.io.handler() else {
            return vec!["sd: this server has no sessions to open a window in".to_owned()];
        };
        match handler
            .spawn_repl_window(&self.io, &self.job, name.map(ShellId::from), &dir, cmd)
            .await
        {
            Ok(id) => vec![format!("{} started", id.reference())],
            Err(error) => vec![format!("sd: {error}")],
        }
    }

    /// Where this job's shell currently stands, relative to its own snapshot.
    ///
    /// A cwd that is not inside the snapshot is an error rather than a silent fall back to the
    /// seed root: `sd api docs` names the `docs` beside the files the prompt is showing, and
    /// answering with a directory at the top of the seed would open the job somewhere the user
    /// never named.
    fn snapshot_relative_cwd(&self) -> Result<String, String> {
        let Some(view) = self.io.job(self.job.id()) else {
            return Err(format!("sd: {}: no such job", self.job.id().reference()));
        };
        let Some(root) = view.snapshot_root.as_ref() else {
            return Err(format!(
                "sd: {}: has no snapshot to resolve a directory against",
                view.id.reference()
            ));
        };
        match view.working_directory.strip_prefix(root) {
            Ok(relative) => Ok(relative.to_string_lossy().into_owned()),
            Err(_) => Err(format!(
                "sd: {}: current directory is outside this job's snapshot",
                view.working_directory.display()
            )),
        }
    }

    /// Closes a named job, gracefully or by force.
    async fn stop_job(&self, args: &[String]) -> Vec<String> {
        let parsed = match repl::parse_stop(args) {
            Ok(parsed) => parsed,
            Err(message) => return vec![message],
        };
        let handle = match self.io.shell(&ShellId::from(parsed.job.as_str())) {
            Ok(handle) => handle,
            Err(error) => return vec![format!("stop: {error}")],
        };
        match self.io.stop(&handle, parsed.force).await {
            Ok(()) => Vec::new(),
            Err(error) => vec![format!("stop: {error}")],
        }
    }
}

/// What a finished line decided.
enum Submission {
    /// The editing session is over.
    Session(Session),
    /// The line is unfinished or empty: prompt again on the same lease.
    Reprompt,
}

/// A repaint when something changed, nothing when it did not.
const fn changed(changed: bool) -> Step {
    if changed { Step::Changed } else { Step::Idle }
}

/// The managed `kill` invocation a typed `kill` becomes.
///
/// Each argument is quoted on its own, so the job control grammar's tokens reach the builtin as
/// the words that were typed rather than being re-split, re-globbed or expanded by the shell that
/// runs the line.
fn kill_line(args: &[String]) -> String {
    let mut line = String::from("kill");
    for arg in args {
        line.push(' ');
        line.push_str(&brush_core::escape::force_quote(
            arg,
            brush_core::escape::QuoteMode::SingleQuote,
        ));
    }
    line
}

/// Where the caret sits: rows below the first painted row, and columns across.
fn caret(prompt: &str, text: &str, cursor: usize, utf8: &Utf8Config) -> (usize, usize) {
    let head = &text[..prompt_buffer::byte_index_for_char(text, cursor)];
    let row = head.matches('\n').count();
    let last = head.rsplit('\n').next().unwrap_or(head);
    let indent = if row == 0 { text_width(prompt, utf8) } else { 0 };
    (row, indent + text_width(last, utf8))
}

/// Whether `bytes` is an Escape that could still turn out to be the head of a sequence.
///
/// Exactly the two prefixes every escape sequence passes through — `ESC`, and the `ESC [` that
/// opens every CSI — because the shared decoder resolves both to the Escape key rather than
/// waiting, and a read that ends on one has genuinely not said which it is yet. A longer prefix
/// is unambiguous enough to keep waiting on without a timer.
fn escape_undecided(bytes: &[u8]) -> bool {
    matches!(bytes, [0x1b] | [0x1b, b'['])
}

/// Whether `bytes` is a strict prefix of `marker`, so more bytes could complete it.
fn is_partial(bytes: &[u8], marker: &[u8]) -> bool {
    bytes.len() < marker.len() && marker.starts_with(bytes)
}

/// How many bytes at the end of `bytes` could be the start of `marker`.
fn trailing_partial(bytes: &[u8], marker: &[u8]) -> usize {
    (1..marker.len())
        .rev()
        .find(|len| bytes.len() >= *len && bytes.ends_with(&marker[..*len]))
        .unwrap_or(0)
}

/// Where `needle` first occurs in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
#[path = "pane_repl/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pane_repl/window_tests.rs"]
mod window_tests;
