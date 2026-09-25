use std::os::fd::BorrowedFd;

use rmux_os::process;

use super::RuntimeFormatContext;

impl RuntimeFormatContext<'_> {
    pub(super) fn pane_foreground_pid(&self) -> Option<u32> {
        let session_name = self.session_name()?;
        let window_index = self.window_index?;
        let pane = self.pane?;
        let state = self.state?;
        state
            .pane_terminal_fd(session_name, window_index, pane.index())
            .ok()
            .and_then(process_foreground_pid)
            .or_else(|| {
                state
                    .pane_pid_in_window(session_name, window_index, pane.index())
                    .ok()
            })
    }

    pub(super) fn pane_current_path(&self) -> Option<String> {
        self.pane_foreground_pid()
            .and_then(process::current_path)
            .or_else(|| self.pane_screen_path())
            .or_else(|| {
                let state = self.state?;
                let session_name = self.session_name()?;
                let window_index = self.window_index?;
                let pane = self.pane?;
                state
                    .pane_profile_in_window(session_name, window_index, pane.index())
                    .ok()
                    .map(|profile| profile.cwd().to_string_lossy().into_owned())
            })
            .or_else(|| self.environment_value_by_name("PWD"))
            .or_else(|| self.environment_value_by_name("HOME"))
    }

    pub(super) fn pane_current_command(&self) -> Option<String> {
        let state = self.state?;
        let session_name = self.session_name()?;
        let window_index = self.window_index?;
        let pane = self.pane?;
        let runtime_name = state
            .pane_runtime_window_name_in_window(session_name, window_index, pane.index())
            .ok()
            .flatten();
        let shell_name = state
            .pane_profile_in_window(session_name, window_index, pane.index())
            .ok()
            .and_then(|profile| {
                profile
                    .shell()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            });
        let foreground_name = self.pane_foreground_pid().and_then(process::command_name);
        match (foreground_name, runtime_name, shell_name) {
            (Some(foreground), Some(runtime), Some(shell))
                if foreground == shell && runtime != shell =>
            {
                Some(runtime)
            }
            (Some(foreground), _, _) => Some(foreground),
            (None, Some(runtime), _) => Some(runtime),
            (None, None, shell) => shell,
        }
    }
}

fn process_foreground_pid(fd: BorrowedFd<'_>) -> Option<u32> {
    process::unix::foreground_pid(fd)
}
