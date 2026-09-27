use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::ExitFailure;
use super::aux_command::{AuxCommand, failure, user_home, write_stdout};

const INSTALL_SKILL_COMMAND: &str = "install-skill";
/// The command every install failure is worded for.
const COMMAND: &str = "claude install-skill";
const SKILL_SOURCE_PATH: &str = "src/rmux/assets/claude-skill.txt";
// The skill text ships as application data next to the CLI that installs it, so the binary does
// not depend on upstream's repository layout being present at build time.
const SKILL_CONTENT: &str = include_str!("../assets/claude-skill.txt");

/// The one `rmux claude` subcommand served here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClaudeSkillInvocation {
    InstallSkill,
}

impl AuxCommand for ClaudeSkillInvocation {
    /// Recognizes `install-skill` as the first argument after `claude`, rejecting any after it.
    fn parse(arguments: &[OsString]) -> Result<Option<Self>, ExitFailure> {
        match arguments {
            [command] if command == INSTALL_SKILL_COMMAND => Ok(Some(Self::InstallSkill)),
            [command, ..] if command == INSTALL_SKILL_COMMAND => {
                Err(ExitFailure::new(1, "usage: rmux claude install-skill"))
            }
            _ => Ok(None),
        }
    }

    /// Dispatches the parsed skill subcommand.
    fn run(self, _argv: &[OsString]) -> Result<i32, ExitFailure> {
        match self {
            Self::InstallSkill => install_skill(),
        }
    }
}

/// Installs the bundled skill text under the user's `.claude` skills directory.
fn install_skill() -> Result<i32, ExitFailure> {
    let path = user_home(COMMAND)?.join(".claude/skills/rmux/SKILL.md");
    let parent = path.parent().ok_or_else(|| {
        failure(
            COMMAND,
            format_args!("invalid Claude skill path '{}'", path.display()),
        )
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        failure(
            COMMAND,
            format_args!("failed to create '{}': {error}", parent.display()),
        )
    })?;

    let status = install_skill_file(&path)?;

    write_stdout(
        format_install_status(&path, status).as_bytes(),
        "claude skill",
    )
}

/// What installing the skill file actually did on disk.
enum InstallSkillStatus {
    Exists,
    Installed,
    Updated { backup: PathBuf },
}

/// Writes the skill at `path`, backing up and replacing any differing regular file.
fn install_skill_file(path: &Path) -> Result<InstallSkillStatus, ExitFailure> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            write_skill_atomic(path)?;
            return Ok(InstallSkillStatus::Installed);
        }
        Err(error) => {
            return Err(failure(
                COMMAND,
                format_args!("failed to inspect '{}': {error}", path.display()),
            ));
        }
    };

    if metadata.file_type().is_symlink() {
        return Err(failure(
            COMMAND,
            format_args!(
                "'{}' is a symlink; refusing to overwrite it",
                path.display()
            ),
        ));
    }
    if !metadata.is_file() {
        return Err(failure(
            COMMAND,
            format_args!("'{}' exists and is not a regular file", path.display()),
        ));
    }

    let existing = fs::read(path).map_err(|error| {
        failure(
            COMMAND,
            format_args!("failed to read '{}': {error}", path.display()),
        )
    })?;
    if existing == SKILL_CONTENT.as_bytes() {
        return Ok(InstallSkillStatus::Exists);
    }

    let backup = backup_existing_skill(path, &existing)?;
    write_skill_atomic(path)?;
    Ok(InstallSkillStatus::Updated { backup })
}

/// Renders the human-readable report line for one install outcome.
fn format_install_status(path: &Path, status: InstallSkillStatus) -> String {
    match status {
        InstallSkillStatus::Exists => format!(
            "exists:     {}\nsource:      {SKILL_SOURCE_PATH}\n",
            path.display()
        ),
        InstallSkillStatus::Installed => format!(
            "installed:  {}\nsource:      {SKILL_SOURCE_PATH}\n",
            path.display()
        ),
        InstallSkillStatus::Updated { backup } => format!(
            "updated:    {}\nbackup:     {}\nsource:      {SKILL_SOURCE_PATH}\n",
            path.display(),
            backup.display()
        ),
    }
}

/// Copies `existing` to the first unused `rmux-backup` sibling of `path`.
fn backup_existing_skill(path: &Path, existing: &[u8]) -> Result<PathBuf, ExitFailure> {
    for candidate in backup_path_candidates(path) {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.write_all(existing).map_err(|error| {
                    failure(
                        COMMAND,
                        format_args!("failed to write backup '{}': {error}", candidate.display()),
                    )
                })?;
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(failure(
                    COMMAND,
                    format_args!("failed to create backup '{}': {error}", candidate.display()),
                ));
            }
        }
    }

    Err(failure(
        COMMAND,
        format_args!("failed to choose a backup path for '{}'", path.display()),
    ))
}

/// Yields up to 1000 `rmux-backup` sibling names for `path`, numbering after the first.
fn backup_path_candidates(path: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    (0..1000).map(move |index| {
        let suffix = if index == 0 {
            "rmux-backup".to_owned()
        } else {
            format!("rmux-backup.{index}")
        };
        path.with_file_name(format!("{}.{suffix}", skill_file_name(path)))
    })
}

/// Writes the skill text to a temporary sibling, then renames it over `path`.
fn write_skill_atomic(path: &Path) -> Result<(), ExitFailure> {
    let temp = path.with_file_name(format!(
        ".{}.rmux-tmp-{}",
        skill_file_name(path),
        std::process::id()
    ));
    fs::write(&temp, SKILL_CONTENT).map_err(|error| {
        failure(
            COMMAND,
            format_args!(
                "failed to write temporary skill '{}': {error}",
                temp.display()
            ),
        )
    })?;

    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        failure(
            COMMAND,
            format_args!("failed to replace '{}': {error}", path.display()),
        )
    })
}

/// The skill file's own name, `SKILL.md` when `path` has no UTF-8 one.
fn skill_file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("SKILL.md")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::ClaudeSkillInvocation;
    use crate::cli::aux_command::{AuxCommand, args};

    #[test]
    fn parses_install_skill_subcommand() {
        assert_eq!(
            ClaudeSkillInvocation::parse(&args(&["install-skill"])).expect("parse succeeds"),
            Some(ClaudeSkillInvocation::InstallSkill)
        );
    }

    #[test]
    fn leaves_regular_claude_args_to_launcher() {
        assert_eq!(
            ClaudeSkillInvocation::parse(&args(&["--dangerously-skip-permissions"]))
                .expect("parse succeeds"),
            None
        );
    }

    #[test]
    fn leaves_delimited_install_skill_arg_to_launcher() {
        assert_eq!(
            ClaudeSkillInvocation::parse(&args(&["--", "install-skill"])).expect("parse succeeds"),
            None
        );
    }

    #[test]
    fn rejects_extra_install_skill_args() {
        let error = ClaudeSkillInvocation::parse(&args(&["install-skill", "--force"]))
            .expect_err("extra args should fail");
        assert_eq!(error.message(), "usage: rmux claude install-skill");
    }
}
