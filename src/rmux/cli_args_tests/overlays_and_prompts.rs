#[test]
fn overlay_client_flags_preserve_list_clients_names() {
    let args = parse_command!(
        DisplayMenu,
        ["display-menu", "-c", "/dev/pts/11", "Item", "i", "true"]
    );
    assert_eq!(args.target_client.as_deref(), Some("/dev/pts/11"));

    let args = parse_command!(DisplayPopup, ["display-popup", "-c", "/dev/pts/12", "true"]);
    assert_eq!(args.target_client.as_deref(), Some("/dev/pts/12"));
}

#[test]
fn display_menu_parses_overlay_flags_and_queue_command() {
    let args = parse_command!(
        DisplayMenu,
        [
            "display-menu",
            "-M",
            "-O",
            "-b",
            "double",
            "-c",
            "%1",
            "-C",
            "2",
            "-H",
            "fg=black",
            "-s",
            "fg=blue",
            "-S",
            "fg=yellow",
            "-t",
            "alpha:0.0",
            "-T",
            "Menu",
            "-x",
            "C",
            "-y",
            "P",
            "Open",
            "o",
            "display-message open",
        ]
    );
    assert!(args.mouse);
    assert!(args.select_open);
    assert_eq!(args.border_lines.as_deref(), Some("double"));
    assert_eq!(args.target_client.as_deref(), Some("%1"));
    assert_eq!(args.starting_choice.as_deref(), Some("2"));
    assert_eq!(args.selected_style.as_deref(), Some("fg=black"));
    assert_eq!(args.style.as_deref(), Some("fg=blue"));
    assert_eq!(args.border_style.as_deref(), Some("fg=yellow"));
    assert_eq!(args.target.as_deref(), Some("alpha:0.0"));
    assert_eq!(args.title.as_deref(), Some("Menu"));
    assert_eq!(args.x.as_deref(), Some("C"));
    assert_eq!(args.y.as_deref(), Some("P"));
    assert_eq!(args.items, vec!["Open", "o", "display-message open"]);
    assert!(args.queue_command.starts_with("display-menu "));
    assert!(args.queue_command.contains("-T Menu"));
    assert!(args.queue_command.contains("display-message open"));
}

#[test]
fn display_popup_parses_overlay_flags_and_queue_command() {
    let args = parse_command!(
        DisplayPopup,
        [
            "display-popup",
            "-B",
            "-C",
            "-E",
            "-k",
            "-N",
            "-b",
            "double",
            "-c",
            "%1",
            "-d",
            "/tmp",
            "-e",
            "FOO=bar",
            "-h",
            "12",
            "-s",
            "fg=blue",
            "-S",
            "fg=yellow",
            "-t",
            "alpha:0.0",
            "-T",
            "Popup",
            "-w",
            "40",
            "-x",
            "C",
            "-y",
            "P",
            "printf hi",
        ]
    );
    assert!(args.no_border);
    assert!(args.close_all);
    assert_eq!(args.close_on_exit, 1);
    assert!(args.close_on_key);
    assert!(args.no_title_border);
    assert_eq!(args.border_lines.as_deref(), Some("double"));
    assert_eq!(args.target_client.as_deref(), Some("%1"));
    assert_eq!(args.start_directory.as_deref(), Some("/tmp"));
    assert_eq!(args.environment, vec!["FOO=bar"]);
    assert_eq!(args.height.as_deref(), Some("12"));
    assert_eq!(args.style.as_deref(), Some("fg=blue"));
    assert_eq!(args.border_style.as_deref(), Some("fg=yellow"));
    assert_eq!(args.target.as_deref(), Some("alpha:0.0"));
    assert_eq!(args.title.as_deref(), Some("Popup"));
    assert_eq!(args.width.as_deref(), Some("40"));
    assert_eq!(args.x.as_deref(), Some("C"));
    assert_eq!(args.y.as_deref(), Some("P"));
    assert_eq!(args.shell_command, vec!["printf hi"]);
    assert!(args.queue_command.starts_with("display-popup "));
    assert!(args.queue_command.contains("-T Popup"));
    assert!(args.queue_command.contains("printf hi"));
}

#[test]
fn prompt_history_commands_parse_optional_type_filters() {
    let args = parse_command!(
        ClearPromptHistory,
        ["clear-prompt-history", "-T", "window-target"]
    );
    assert_eq!(args.prompt_type.as_deref(), Some("window-target"));
    assert_eq!(args.queue_command, "clear-prompt-history -T window-target");

    let args = parse_command!(ShowPromptHistory, ["show-prompt-history", "-T", "search"]);
    assert_eq!(args.prompt_type.as_deref(), Some("search"));
    assert_eq!(args.queue_command, "show-prompt-history -T search");
}

#[test]
fn prompt_commands_accept_target_client_flags() {
    let args = parse_command!(
        Prompt,
        [
            "command-prompt",
            "-t",
            "99999",
            "-p",
            "name",
            "display-message hi",
        ]
    );
    assert_eq!(args.target_client.as_deref(), Some("99999"));
    assert_eq!(args.prompts.as_deref(), Some("name"));
    assert!(args.queue_command.contains("-t 99999"));

    let args = parse_command!(
        ConfirmBefore,
        [
            "confirm-before",
            "-t",
            "99999",
            "-p",
            "sure",
            "display-message hi",
        ]
    );
    assert_eq!(args.target_client.as_deref(), Some("99999"));
    assert_eq!(args.prompt.as_deref(), Some("sure"));
    assert!(args.queue_command.contains("-t 99999"));
}

#[test]
fn command_prompt_accepts_tmux_command_error_backspace_and_literal_flags() {
    let args = parse_command!(Prompt, ["command-prompt", "-Cel", "display-message hi"]);
    assert!(args.command_error);
    assert!(args.backspace_exit);
    assert!(args.literal);
    assert_eq!(args.template, vec!["display-message hi"]);
}
