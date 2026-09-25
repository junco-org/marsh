use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rmux_core::OptionStore;
use rmux_proto::{OptionName, SessionName};
use rustix::fs::{access, Access};
use rustix::process::getuid;

/// The `default-shell` a pane was deliberately configured with, if it has one.
///
/// Deliberately *not* [`resolve_shell_path`]. That function answers "which shell do helper
/// commands run through", and always answers with a path: it falls back to `$SHELL`, then to the
/// user's login shell, then to `/bin/sh`. A pane's mode is a different question with a real
/// negative answer — no configured shell means the embedded interpreter, not `/bin/sh` — and
/// reading it off the resolved helper path would turn every unconfigured pane into a long-lived
/// `sh` subprocess that gates a whole session instead of each submitted line.
///
/// The suitability filter is shared with [`resolve_shell_path`] on purpose: one daemon must not
/// hold two opinions about whether a configured shell is usable.
pub(super) fn configured_pane_shell(
    options: &OptionStore,
    session_name: Option<&SessionName>,
) -> Option<PathBuf> {
    session_name
        .and_then(|session_name| options.resolve(Some(session_name), OptionName::DefaultShell))
        .or_else(|| options.resolve(None, OptionName::DefaultShell))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| is_suitable_shell(path))
        .map(normalize_shell_path)
}

pub(super) fn resolve_shell_path(
    options: &OptionStore,
    session_name: Option<&SessionName>,
    environment: &HashMap<String, String>,
) -> PathBuf {
    session_name
        .and_then(|session_name| options.resolve(Some(session_name), OptionName::DefaultShell))
        .or_else(|| options.resolve(None, OptionName::DefaultShell))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| is_suitable_shell(path))
        .map(normalize_shell_path)
        .or_else(|| {
            environment
                .get("SHELL")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .filter(|path| is_suitable_shell(path))
        })
        .or_else(|| current_user_login_shell().filter(|path| is_suitable_shell(path)))
        .map(normalize_shell_path)
        .unwrap_or_else(default_shell_path)
}

pub(crate) fn is_suitable_shell(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && access(path, Access::EXEC_OK).is_ok()
}

pub(super) fn normalize_shell_path(path: PathBuf) -> PathBuf {
    path
}

fn current_user_login_shell() -> Option<PathBuf> {
    let uid = getuid().as_raw();
    fs::read_to_string("/etc/passwd")
        .ok()?
        .lines()
        .find_map(|line| passwd_shell_for_uid(line, uid))
}

fn passwd_shell_for_uid(line: &str, uid: u32) -> Option<PathBuf> {
    let mut fields = line.split(':');
    let _name = fields.next()?;
    let _password = fields.next()?;
    let parsed_uid = fields.next()?.parse::<u32>().ok()?;
    let _gid = fields.next()?;
    let _gecos = fields.next()?;
    let _home = fields.next()?;
    let shell = fields.next()?;
    (parsed_uid == uid && !shell.is_empty()).then(|| PathBuf::from(shell))
}

fn default_shell_path() -> PathBuf {
    PathBuf::from("/bin/sh")
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    #[test]
    fn invalid_shell_environment_falls_back_to_a_suitable_login_shell() {
        let environment = HashMap::from([(
            "SHELL".to_owned(),
            "/definitely/missing/rmux-shell".to_owned(),
        )]);

        let resolved = resolve_shell_path(&OptionStore::new(), None, &environment);

        assert_ne!(resolved, PathBuf::from(&environment["SHELL"]));
        assert!(is_suitable_shell(&resolved), "{resolved:?}");
    }

    #[test]
    fn invalid_stored_default_shell_falls_back_to_the_session_environment() {
        let mut options = OptionStore::new();
        options
            .set(
                rmux_proto::ScopeSelector::Global,
                OptionName::DefaultShell,
                "/definitely/missing/rmux-shell".to_owned(),
                rmux_proto::SetOptionMode::Replace,
            )
            .expect("core option storage accepts string values");
        let environment = HashMap::from([("SHELL".to_owned(), "/bin/sh".to_owned())]);

        assert_eq!(
            resolve_shell_path(&options, None, &environment),
            PathBuf::from("/bin/sh")
        );
    }
}
