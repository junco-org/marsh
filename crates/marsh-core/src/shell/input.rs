//! Private Brush editor adapter. Shell owns every prompt/line/finalization span; input never replays.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use brush_interactive::{
    BasicInputBackend, InputBackend, InteractivePrompt, InteractiveShell, ReadResult,
};
use marsh_lib::RecoverPoison as _;

use super::completion::Completion;
use super::snapshot::{PreparedCommand, Snapshot};
use super::{Command, ExecutionResult, Live, Shared, Shell, ShellError, ShellErrorKind, UIOptions};

pub(super) async fn run(shell: &Shell, options: UIOptions) -> Result<ExecutionResult, ShellError> {
    if shell.shared.runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
        return Err(ShellError::unsupported(
            "the blocking editor requires a multi-thread runtime",
        ));
    }
    shell.execute(Command::Interactive(options)).await
}

struct Adapter<'a> {
    backend: BasicInputBackend,
    owner: &'a Shared,
    snapshot: Arc<Snapshot>,
    pending: Option<Completion<PreparedCommand>>,
    command: String,
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
        let pending = self.pending.take().ok_or_else(|| {
            interactive_error(ShellError::infrastructure("interactive span missing"))
        })?;
        let command = std::mem::take(&mut self.command);
        let owner = self.owner;
        let internal = self.snapshot.session.tracing.internal_scope()?;
        let _internal = internal.enter();
        let finished = tokio::task::block_in_place(|| {
            owner.runtime.block_on(owner.finish_span(
                pending,
                ExecutionResult::new(status),
                command,
            ))
        });
        if let Err(error) = finished {
            match error.kind() {
                ShellErrorKind::Denied { .. }
                | ShellErrorKind::Stale { .. }
                | ShellErrorKind::Unsupported => eprintln!("marsh: {error}"),
                _ => return Err(interactive_error(error)),
            }
        }
        self.pending = Some(
            owner
                .begin_span(&self.snapshot)
                .map_err(interactive_error)?,
        );
        if owner.force.load(Ordering::Acquire) {
            return Ok(ReadResult::Eof);
        }
        let read = self.backend.read_line(shell, prompt)?;
        if let ReadResult::Input(line) | ReadResult::BoundCommand(line) = &read {
            self.command.clone_from(line);
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

pub(super) async fn run_owned(
    owner: &Shared,
    live: &mut Live,
    options: UIOptions,
) -> Result<ExecutionResult, ShellError> {
    let prepared = owner.begin_span(&live.snapshot)?;
    // Move the live interpreter, not a clone of its state/descriptors, into the editor's private
    // reference. No caller receives this handle; admission remains owned by the enclosing Shell.
    let interpreter = Arc::new(tokio::sync::Mutex::new(std::mem::take(
        &mut live.interpreter,
    )));
    let mut adapter = Adapter {
        backend: BasicInputBackend,
        owner,
        snapshot: Arc::clone(&live.snapshot),
        pending: Some(prepared),
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
    let pending = adapter
        .pending
        .take()
        .ok_or_else(|| ShellError::infrastructure("final editor span missing"))?;
    let command = std::mem::take(&mut adapter.command);
    let failure = outcome
        .err()
        .map(|error| ShellError::infrastructure(error.to_string()));
    let executed = super::execution::complete(pending, Some(native), failure, command).await;
    *owner.active.lock().recover() = std::sync::Weak::new();
    Shared::publish_span(&live.snapshot.session, executed)
}
fn interactive_error(error: ShellError) -> brush_interactive::ShellError {
    brush_interactive::ShellError::IoError(std::io::Error::other(error))
}
