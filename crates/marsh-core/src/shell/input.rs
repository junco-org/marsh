//! Private Brush editor adapter. Shell owns every prompt/line/completion/finalization span; input
//! never replays, and no admission is held while the editor waits for a key.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use brush_interactive::{
    BasicInputBackend, Completions, InputBackend, InteractivePrompt, InteractiveShell, ReadResult,
};

use super::execution::Run;
use super::session::ExecutionResources;
use super::{
    Action, Command, ExecutionResult, Live, Shared, Shell, ShellCommand, ShellError,
    ShellErrorKind, Span, SpanRoute, UIOptions,
};

pub(super) async fn run(shell: &Shell, options: UIOptions) -> Result<ExecutionResult, ShellError> {
    if shell.shared.runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
        return Err(ShellError::unsupported(
            "the blocking editor requires a multi-thread runtime",
        ));
    }
    shell.execute(Command::Interactive(options), None).await
}

struct Adapter<'a> {
    backend: BasicInputBackend,
    owner: &'a Shared,
    resources: &'a mut ExecutionResources,
    parent: Option<Weak<Run>>,
    /// The span covering the last accepted input and the prompt work after it.
    pending: Option<Span>,
    /// That span's accepted text, kept only when a managed publication records it.
    command: String,
}
impl Adapter<'_> {
    /// Begins the span for `text` on the locked interpreter, keeping its text when managed.
    fn begin(
        &mut self,
        shell: &brush_interactive::ShellRef<impl brush_core::ShellExtensions>,
        text: &str,
    ) -> Result<(), ShellError> {
        let owner = self.owner;
        let tool = ShellCommand {
            command: text.to_owned(),
        };
        let action: Action = (&tool).into();
        let span = tokio::task::block_in_place(|| {
            owner.runtime.block_on(async {
                let mut interpreter = shell.lock().await;
                owner
                    .begin_span(
                        &mut *interpreter,
                        &mut *self.resources,
                        &tool,
                        &action,
                        self.parent.as_ref(),
                        None,
                    )
                    .await
            })
        })?;
        if matches!(span.route, SpanRoute::Managed(_)) {
            self.command = tool.command;
        }
        self.pending = Some(span);
        Ok(())
    }
}
impl InputBackend for Adapter<'_> {
    fn read_line(
        &mut self,
        shell: &brush_interactive::ShellRef<impl brush_core::ShellExtensions>,
        prompt: InteractivePrompt,
    ) -> Result<ReadResult, brush_interactive::ShellError> {
        let status = shell
            .try_lock()
            .map_err(|_| {
                interactive_error(ShellError::infrastructure(
                    "editor boundary holds interpreter lock",
                ))
            })?
            .last_exit_status();
        let owner = self.owner;
        if let Some(span) = self.pending.take() {
            let command = std::mem::take(&mut self.command);
            let resources = &*self.resources;
            let finished = tokio::task::block_in_place(|| {
                owner.runtime.block_on(owner.finish_span(
                    resources,
                    span,
                    ExecutionResult::new(status),
                    None,
                    command,
                ))
            });
            report(finished).map_err(interactive_error)?;
        }
        if owner.force.load(Ordering::Acquire) {
            return Ok(ReadResult::Eof);
        }
        let Self {
            backend,
            resources,
            parent,
            ..
        } = self;
        let read =
            backend.read_line_with_completion(shell, &prompt, |interpreter, line, cursor| {
                complete(owner, resources, parent.as_ref(), interpreter, line, cursor)
            })?;
        match &read {
            ReadResult::Input(line) | ReadResult::BoundCommand(line) => {
                if let Err(error) = self.begin(shell, line) {
                    // The line never ran; its prompt still gets its own span.
                    eprintln!("marsh: {error}");
                    let _ = self.begin(shell, "");
                    return Ok(ReadResult::Interrupted);
                }
            }
            ReadResult::Interrupted | ReadResult::Eof => {
                if let Err(error) = self.begin(shell, "") {
                    eprintln!("marsh: {error}");
                }
            }
        }
        Ok(read)
    }
    fn get_read_buffer(&self) -> Option<(String, usize)> {
        self.backend.get_read_buffer()
    }
    fn set_read_buffer(&mut self, buffer: String, cursor: usize) {
        self.backend.set_read_buffer(buffer, cursor);
    }
}

/// Answers one completion request as its own span, admitted with the exact line being completed.
fn complete<SE: brush_core::ShellExtensions>(
    owner: &Shared,
    resources: &mut ExecutionResources,
    parent: Option<&Weak<Run>>,
    interpreter: &mut brush_core::Shell<SE>,
    line: &str,
    cursor: usize,
) -> Result<Completions, brush_interactive::ShellError> {
    let tool = ShellCommand {
        command: line.to_owned(),
    };
    let action: Action = (&tool).into();
    let span = tokio::task::block_in_place(|| {
        owner.runtime.block_on(owner.begin_span(
            interpreter,
            resources,
            &tool,
            &action,
            parent,
            None,
        ))
    });
    let span = match span {
        Ok(span) => span,
        Err(error) => {
            eprintln!("marsh: {error}");
            return Ok(Completions {
                insertion_index: cursor,
                delete_count: 0,
                candidates: Vec::new(),
                options: brush_core::completion::ProcessingOptions::default(),
            });
        }
    };
    let command = if matches!(span.route, SpanRoute::Managed(_)) {
        tool.command
    } else {
        String::new()
    };
    let completions = BasicInputBackend::generate_completions(interpreter, line, cursor);
    let status = interpreter.last_exit_status();
    let resources = &*resources;
    let finished = tokio::task::block_in_place(|| {
        owner.runtime.block_on(owner.finish_span(
            resources,
            span,
            ExecutionResult::new(status),
            None,
            command,
        ))
    });
    report(finished).map_err(interactive_error)?;
    completions
}

/// A refused span is reported and the session continues; any other failure ends it.
fn report(finished: Result<ExecutionResult, ShellError>) -> Result<(), ShellError> {
    match finished {
        Ok(_) => Ok(()),
        Err(error) => match error.kind() {
            ShellErrorKind::Denied { .. }
            | ShellErrorKind::Stale { .. }
            | ShellErrorKind::Unsupported => {
                eprintln!("marsh: {error}");
                Ok(())
            }
            _ => Err(error),
        },
    }
}

pub(super) async fn run_owned(
    owner: &Shared,
    live: &mut Live,
    options: UIOptions,
    parent: Option<Weak<Run>>,
) -> Result<ExecutionResult, ShellError> {
    let tool = ShellCommand {
        command: String::new(),
    };
    let action: Action = (&tool).into();
    let span = owner
        .begin_span(
            &mut live.interpreter,
            &mut live.resources,
            &tool,
            &action,
            parent.as_ref(),
            None,
        )
        .await?;
    // Move the live interpreter, not a clone of its state/descriptors, into the editor's private
    // reference. No caller receives this handle; admission remains owned by the enclosing Shell.
    let interpreter = Arc::new(tokio::sync::Mutex::new(std::mem::take(
        &mut live.interpreter,
    )));
    let mut adapter = Adapter {
        backend: BasicInputBackend,
        owner,
        resources: &mut live.resources,
        parent,
        pending: Some(span),
        command: String::new(),
    };
    let outcome = match InteractiveShell::new(&interpreter, &mut adapter, &(&options).into()) {
        Ok(mut editor) => editor.run_interactively().await,
        Err(error) => Err(error),
    };
    live.interpreter = std::mem::take(&mut *interpreter.lock().await);
    drop(interpreter);
    owner.closed.store(true, Ordering::Release);
    let mut native = ExecutionResult::new(live.interpreter.last_exit_status());
    native.next_control_flow = brush_core::ExecutionControlFlow::ExitShell;
    let failure = outcome
        .err()
        .map(|error| ShellError::infrastructure(error.to_string()));
    let command = std::mem::take(&mut adapter.command);
    match adapter.pending.take() {
        Some(span) => {
            owner
                .finish_span(adapter.resources, span, native, failure, command)
                .await
        }
        None => Err(failure.unwrap_or_else(|| ShellError::new(ShellErrorKind::Interrupted))),
    }
}
fn interactive_error(error: ShellError) -> brush_interactive::ShellError {
    brush_interactive::ShellError::IoError(std::io::Error::other(error))
}
