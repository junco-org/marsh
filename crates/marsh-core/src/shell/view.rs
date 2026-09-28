//! Moving the persistent interpreter between the source and a private managed view.
//!
//! Only the working directory and the `PWD`/`OLDPWD` variables are re-expressed in the target
//! view. Descriptors bound inside the outgoing tree are revoked, never reopened elsewhere: a later
//! use gets ordinary bad-descriptor behavior, and the command may reopen the path explicitly.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use brush_core::env::{EnvironmentLookup, ShellEnvironment};
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::{ExecutionParameters, ShellFd, ShellVariable};

use super::ShellError;

/// `path` under `to`, when it lies under `from`; otherwise `path` unchanged.
pub(super) fn remap(path: &Path, from: &Path, to: &Path) -> PathBuf {
    path.strip_prefix(from)
        .map_or_else(|_| path.to_path_buf(), |relative| to.join(relative))
}

/// A directory variable re-expressed from `from` to `to`, with every attribute it had.
pub(super) fn remap_variable(
    name: &str,
    variable: ShellVariable,
    from: &Path,
    to: &Path,
) -> ShellVariable {
    if !matches!(name, "PWD" | "OLDPWD") || variable.is_treated_as_nameref() {
        return variable;
    }
    let brush_core::ShellValue::String(value) = variable.value() else {
        return variable;
    };
    if !Path::new(value).starts_with(from) {
        return variable;
    }
    let mut mapped = ShellVariable::new(
        remap(Path::new(value), from, to)
            .to_string_lossy()
            .into_owned(),
    );
    if variable.is_exported() {
        mapped.export();
    }
    if variable.is_readonly() {
        mapped.set_readonly();
    }
    if variable.is_trace_enabled() {
        mapped.enable_trace();
    }
    if !variable.is_enumerable() {
        mapped.hide_from_enumeration();
    }
    if variable.is_treated_as_integer() {
        mapped.treat_as_integer();
    }
    mapped.set_update_transform(variable.get_update_transform());
    mapped
}

/// Re-expresses `PWD` and `OLDPWD` in place, bypassing assignment so read-only ones move too.
pub(super) fn remap_environment(environment: &mut ShellEnvironment, from: &Path, to: &Path) {
    for name in ["PWD", "OLDPWD"] {
        if let Some(variable) = environment.get_mut_using_policy(name, EnvironmentLookup::Anywhere)
        {
            *variable = remap_variable(name, std::mem::take(variable), from, to);
        }
    }
}

/// Descriptor numbers of `files` whose file or directory lies inside `tree`.
fn inside(files: &OpenFiles, tree: &Path) -> Result<Vec<ShellFd>, ShellError> {
    let mut found = Vec::new();
    for (fd, file) in files.iter_fds() {
        let OpenFile::File(file) = file else {
            continue;
        };
        if std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?.starts_with(tree) {
            found.push(fd);
        }
    }
    Ok(found)
}

/// Refuses caller-supplied parameters that would carry a descriptor into a private work view.
pub(super) fn refuse_private(
    params: Option<&ExecutionParameters>,
    private: &Path,
) -> Result<(), ShellError> {
    if let Some(params) = params
        && !inside(params.open_files(), private)?.is_empty()
    {
        return Err(ShellError::unsupported(
            "execution parameters hold a descriptor into a private work view",
        ));
    }
    Ok(())
}

/// A validated move of the interpreter from one view to another; planning mutates nothing.
pub(super) struct Transition {
    cwd: PathBuf,
    from: PathBuf,
    to: PathBuf,
    revoked: Vec<ShellFd>,
}
impl Transition {
    /// Plans the move of everything under `from` to `to`, checking the target directory exists
    /// and collecting the descriptors bound inside `from`.
    pub fn plan<SE: brush_core::ShellExtensions>(
        interpreter: &brush_core::Shell<SE>,
        from: &Path,
        to: &Path,
    ) -> Result<Self, ShellError> {
        let cwd = remap(interpreter.working_dir(), from, to);
        if !std::fs::metadata(&cwd)?.is_dir() {
            return Err(ShellError::infrastructure(
                "working directory is not a directory in the target view",
            ));
        }
        Ok(Self {
            revoked: inside(interpreter.open_files(), from)?,
            cwd,
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        })
    }

    /// Moves the stored directory, then the directory variables, and revokes stale descriptors.
    pub fn apply<SE: brush_core::ShellExtensions>(
        self,
        interpreter: &mut brush_core::Shell<SE>,
    ) -> Result<(), ShellError> {
        interpreter.relocate_working_dir(&self.cwd)?;
        remap_environment(interpreter.env_mut(), &self.from, &self.to);
        for fd in self.revoked {
            interpreter.open_files_mut().remove_fd(fd);
        }
        Ok(())
    }
}
