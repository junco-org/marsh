use super::source_files::{default_config_paths, default_tmux_fallback_paths};
use crate::test_env::EnvVarGuard;

#[test]
fn default_config_paths_use_rmux_locations() {
    let _lock = crate::test_env::lock_blocking();
    let _home = EnvVarGuard::set("HOME", Some("/tmp/rmux-home"));
    let _xdg = EnvVarGuard::set("XDG_CONFIG_HOME", Some("/tmp/rmux-xdg"));

    let paths = default_config_paths();

    assert_eq!(
        paths,
        vec![
            "/etc/rmux.conf".to_owned(),
            "/tmp/rmux-home/.rmux.conf".to_owned(),
            "/tmp/rmux-xdg/rmux/rmux.conf".to_owned(),
            "/tmp/rmux-home/.config/rmux/rmux.conf".to_owned(),
        ]
    );
    assert!(
        paths.iter().all(|path| !path.contains("tmux")),
        "default config search path must not include tmux locations: {paths:?}"
    );
}

#[test]
fn tmux_fallback_paths_use_tmux_locations() {
    let _lock = crate::test_env::lock_blocking();
    let _disable = EnvVarGuard::set("RMUX_DISABLE_TMUX_FALLBACK", None);
    let _home = EnvVarGuard::set("HOME", Some("/tmp/rmux-home"));
    let _xdg = EnvVarGuard::set("XDG_CONFIG_HOME", Some("/tmp/rmux-xdg"));

    let paths = default_tmux_fallback_paths();

    assert_eq!(
        paths,
        vec![
            "/etc/tmux.conf".to_owned(),
            "/tmp/rmux-home/.tmux.conf".to_owned(),
            "/tmp/rmux-xdg/tmux/tmux.conf".to_owned(),
            "/tmp/rmux-home/.config/tmux/tmux.conf".to_owned(),
        ]
    );
    assert!(
        paths.iter().all(|path| !path.ends_with("rmux.conf")),
        "tmux fallback paths must not include rmux config files: {paths:?}"
    );
}

#[test]
fn tmux_fallback_paths_can_be_disabled_by_env() {
    let _lock = crate::test_env::lock_blocking();
    let _disable = EnvVarGuard::set("RMUX_DISABLE_TMUX_FALLBACK", Some("1"));

    assert!(default_tmux_fallback_paths().is_empty());
}
