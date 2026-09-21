//! Process management utilities

pub(crate) type ProcessId = i32;
pub use tokio::process::Child;

// `kill_on_drop`: the shell always passes false here; brush-core 0.5.0 has no
// `CreateOptions::kill_external_commands_on_drop`.
pub(crate) fn spawn(command: std::process::Command, kill_on_drop: bool) -> std::io::Result<Child> {
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(kill_on_drop);
    command.spawn()
}
