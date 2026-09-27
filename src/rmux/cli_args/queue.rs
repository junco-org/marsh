use std::ffi::{OsStr, OsString};

use rmux_core::command_parser::{
    CommandArgument, CommandParseError, CommandParser as TmuxCommandParser, ParsedCommand,
    ParsedCommands,
};

use super::pane::{parse_resize_pane_args, parse_split_window_args};
use super::script::parse_source_file_args;
use super::validate::{
    Validate, invalid_utf8_error, too_many_arguments_error, unknown_flag_error, value_error,
};
use super::web::parse_web_share_args;
use super::window::parse_rename_window_args;
use super::{
    ChooseBufferArgs, ChooseClientArgs, ChooseTreeArgs, Command, ConfirmBeforeArgs,
    CustomizeModeArgs, DisplayMenuArgs, DisplayMessageArgs, DisplayPopupArgs, FindWindowArgs,
    IfShellArgs, NewWindowArgs, PositionalOptionPolicy, PromptArgs, PromptHistoryArgs,
    RuntimeCommandGroup, SendKeysArgs, ServerAccessArgs, SetOptionArgs, SetOptionCommandKind,
    SetWindowOptionArgs, ShowWindowOptionsArgs, UnsupportedCommandArgs, documented_cli_aliases,
    help_argument, parse_command_args, parse_command_args_with_policy,
};

/// Splits raw command-line words into the queue of `tmux`-style commands they encode.
pub(super) fn parse_command_queue(arguments: &[OsString]) -> Result<ParsedCommands, clap::Error> {
    if arguments.is_empty() {
        return Ok(ParsedCommands::default());
    }

    let arguments = arguments
        .iter()
        .map(|argument| command_argument_to_string(argument))
        .collect::<Result<Vec<_>, _>>()?;
    let arguments = expand_cli_argument_aliases(arguments);
    TmuxCommandParser::new()
        .with_exact_commands(super::RMUX_EXTENSION_COMMANDS)
        .parse_arguments(&arguments)
        .map_err(|error| command_parse_error_to_clap(&error))
}

/// Parses rendered runtime command groups into one flat queue, without alias expansion.
pub(super) fn parse_runtime_command_groups(
    groups: &[RuntimeCommandGroup],
) -> Result<ParsedCommands, clap::Error> {
    let parser = TmuxCommandParser::new()
        .with_command_aliases(std::iter::empty::<String>())
        .with_exact_commands(super::RMUX_EXTENSION_COMMANDS);
    let mut parsed = ParsedCommands::default();

    for group in groups {
        let RuntimeCommandGroup::Canonical(rendered) = group;
        if rendered.is_empty() {
            continue;
        }
        let commands = parser
            .parse_one_group(rendered)
            .map_err(|error| command_parse_error_to_clap(&error))?;
        parsed.append(commands);
    }

    Ok(parsed)
}

/// Rewrites the documented built-in aliases, such as `choose-window`, into their expansions.
fn expand_cli_argument_aliases(arguments: Vec<String>) -> Vec<String> {
    let mut expanded = Vec::with_capacity(arguments.len() + 2);
    let mut command_start = true;

    for argument in arguments {
        let (base, ends_command) = split_cli_command_terminator(&argument);
        let alias = documented_cli_aliases()
            .iter()
            .find(|alias| command_start && alias.alias == base);
        command_start = ends_command;
        let Some(alias) = alias else {
            expanded.push(argument);
            continue;
        };
        expanded.extend(alias.expansion.split(' ').map(str::to_owned));
        if let Some(last) = expanded.last_mut().filter(|_| ends_command) {
            last.push(';');
        }
    }

    expanded
}

/// Splits a trailing unescaped `;` off a word, reporting whether it ends the current command.
fn split_cli_command_terminator(argument: &str) -> (&str, bool) {
    if let Some(stripped) = argument.strip_suffix(';') {
        if stripped.ends_with('\\') {
            (argument, false)
        } else {
            (stripped, true)
        }
    } else {
        (argument, false)
    }
}

/// Converts one raw OS argument to UTF-8, failing with a `clap` invalid-UTF-8 error.
fn command_argument_to_string(argument: &OsStr) -> Result<String, clap::Error> {
    argument
        .to_str()
        .map(str::to_owned)
        .ok_or_else(invalid_utf8_error)
}

/// Converts a command-parser failure into a `clap` error with the matching error kind.
fn command_parse_error_to_clap(error: &CommandParseError) -> clap::Error {
    let message = cli_command_error_message(error.message());

    let kind =
        if message.starts_with("unknown command: ") || message.starts_with("ambiguous command: ") {
            clap::error::ErrorKind::InvalidSubcommand
        } else {
            clap::error::ErrorKind::ValueValidation
        };
    clap::Error::raw(kind, message.to_owned())
}

/// Strips the parser's `-:<line>: ` position prefix, which is meaningless for command-line input.
fn cli_command_error_message(message: &str) -> &str {
    let original = message;
    let Some(rest) = original.strip_prefix("-:") else {
        return message;
    };
    let Some((line, stripped)) = rest.split_once(": ") else {
        return original;
    };
    if line.bytes().all(|byte| byte.is_ascii_digit()) {
        stripped
    } else {
        original
    }
}

/// Argument structs that record the raw command text queued for them.
trait QueuedCommand {
    /// Records the original command-queue text this invocation came from.
    fn set_queue_command(&mut self, queue_command: String);
}

/// Implements [`QueuedCommand`] for argument structs that carry a `queue_command` field.
macro_rules! queued_commands {
    ($($args:ty),+ $(,)?) => {$(
        impl QueuedCommand for $args {
            fn set_queue_command(&mut self, queue_command: String) {
                self.queue_command = queue_command;
            }
        }
    )+};
}

queued_commands! {
    NewWindowArgs, FindWindowArgs, DisplayMessageArgs, IfShellArgs, PromptArgs, ConfirmBeforeArgs,
    PromptHistoryArgs, ChooseTreeArgs, ChooseBufferArgs, ChooseClientArgs, CustomizeModeArgs,
    DisplayMenuArgs, DisplayPopupArgs,
}

/// Attaches the original command text to parsed arguments so the server can re-parse it later.
fn with_queue_command<T: QueuedCommand>(mut args: T, queue_command: String) -> T {
    args.set_queue_command(queue_command);
    args
}

/// Parses one row of `dispatch_table!`: `plain` rows parse with clap alone, `checked` rows then
/// run [`Validate::validate`], `queued` rows record the command's reparse text for the server,
/// and `queued_checked` rows do both.
macro_rules! dispatch_row {
    (plain $command:literal, $variant:ident, $arguments:ident, $queue_command:ident) => {
        parse_command_args($command, $arguments).map(Command::$variant)
    };
    (checked $command:literal, $variant:ident, $arguments:ident, $queue_command:ident) => {
        parse_command_args($command, $arguments)
            .and_then(|args| Validate::validate(args, $command))
            .map(Command::$variant)
    };
    (queued $command:literal, $variant:ident, $arguments:ident, $queue_command:ident) => {
        parse_command_args($command, $arguments)
            .map(|args| Command::$variant(with_queue_command(args, $queue_command)))
    };
    (queued_checked $command:literal, $variant:ident, $arguments:ident, $queue_command:ident) => {
        parse_command_args($command, $arguments)
            .and_then(|args| Validate::validate(args, $command))
            .map(|args| Command::$variant(with_queue_command(args, $queue_command)))
    };
}

/// Expands the command dispatch table into one `match` on the command name. Each row names the
/// command and its aliases, how `dispatch_row!` parses it, and its [`Command`] variant; the arms
/// after the `;` handle commands with bespoke parsers and pass through unchanged.
macro_rules! dispatch_table {
    (
        $name:expr, $arguments:ident, $queue_command:ident;
        $($command:literal $(| $alias:literal)* => $kind:ident $variant:ident,)*
        ; $($arm:tt)*
    ) => {
        match $name {
            $($command $(| $alias)* => {
                dispatch_row!($kind $command, $variant, $arguments, $queue_command)
            })*
            $($arm)*
        }
    };
}

/// Dispatches one parsed command name to its argument parser and builds the typed `Command`.
#[allow(
    clippy::too_many_lines,
    reason = "one flat dispatch table over every tmux command name is clearer than arbitrary splits"
)]
pub(super) fn command_from_parsed(command: &ParsedCommand) -> Result<Command, clap::Error> {
    let name = command.name().to_owned();
    let error_command_name = name.clone();
    let queue_command = command.to_tmux_reparse_string();
    let arguments = command_arguments_for_clap(command.arguments());
    let parsed = dispatch_table! { name.as_str(), arguments, queue_command;
        "new-session" => plain NewSession,
        "start-server" => plain StartServer,
        "has-session" => plain HasSession,
        "kill-session" => plain KillSession,
        "rename-session" => plain RenameSession,
        "lock-session" => plain LockSession,
        "lock-client" => plain LockClient,
        "new-window" => queued NewWindow,
        "kill-window" => plain KillWindow,
        "select-window" => checked SelectWindow,
        "next-window" => plain NextWindow,
        "previous-window" => plain PreviousWindow,
        "last-window" => plain LastWindow,
        "list-sessions" => plain ListSessions,
        "list-windows" => plain ListWindows,
        "move-window" => plain MoveWindow,
        "swap-window" => checked SwapWindow,
        "rotate-window" => plain RotateWindow,
        "resize-window" => plain ResizeWindow,
        "respawn-window" => plain RespawnWindow,
        "swap-pane" => plain SwapPane,
        "last-pane" => plain LastPane,
        "join-pane" => plain JoinPane,
        "move-pane" => plain MovePane,
        "break-pane" => plain BreakPane,
        "pipe-pane" => plain PipePane,
        "respawn-pane" => plain RespawnPane,
        "kill-pane" => plain KillPane,
        "select-layout" => plain SelectLayout,
        "next-layout" => plain NextLayout,
        "previous-layout" => plain PreviousLayout,
        "display-panes" => plain DisplayPanes,
        "list-panes" => plain ListPanes,
        "select-pane" => checked SelectPane,
        "copy-mode" => plain CopyMode,
        "clock-mode" => plain ClockMode,
        "wait-pane" => checked WaitPane,
        "pane-snapshot" => plain PaneSnapshot,
        "stream-pane" => checked StreamPane,
        "collect-pane-output" => checked CollectPaneOutput,
        "locator" => checked Locator,
        "expect-pane" => checked ExpectPane,
        "find-panes" => plain FindPanes,
        "find-sessions" => plain FindSessions,
        "broadcast-keys" => checked BroadcastKeys,
        "bind-key" => plain BindKey,
        "unbind-key" => plain UnbindKey,
        "list-commands" => plain ListCommands,
        "list-keys" => plain ListKeys,
        "send-prefix" => plain SendPrefix,
        "attach-session" => plain AttachSession,
        "refresh-client" => plain RefreshClient,
        "list-clients" => plain ListClients,
        "switch-client" => plain SwitchClient,
        "detach-client" => plain DetachClient,
        "suspend-client" => plain SuspendClient,
        "set-environment" => plain SetEnvironment,
        "show-options" => plain ShowOptions,
        "show-environment" => plain ShowEnvironment,
        "set-hook" => plain SetHook,
        "show-hooks" => plain ShowHooks,
        "set-buffer" => checked SetBuffer,
        "show-buffer" => plain ShowBuffer,
        "paste-buffer" => plain PasteBuffer,
        "list-buffers" => plain ListBuffers,
        "delete-buffer" => plain DeleteBuffer,
        "load-buffer" => plain LoadBuffer,
        "save-buffer" => plain SaveBuffer,
        "capture-pane" => plain CapturePane,
        "clear-history" => plain ClearHistory,
        "display-message" => queued_checked DisplayMessage,
        "show-messages" => plain ShowMessages,
        "run-shell" => plain RunShell,
        "if-shell" => queued IfShell,
        "wait-for" => plain WaitFor,
        "command-prompt" => queued Prompt,
        "confirm-before" => queued ConfirmBefore,
        "find-window" => queued FindWindow,
        "link-window" => plain LinkWindow,
        "unlink-window" => plain UnlinkWindow,
        "choose-tree" => queued_checked ChooseTree,
        "choose-buffer" => queued_checked ChooseBuffer,
        "choose-client" => queued_checked ChooseClient,
        "customize-mode" => queued CustomizeMode,
        "display-menu" | "menu" => queued DisplayMenu,
        "display-popup" | "popup" => queued DisplayPopup,
        "clear-prompt-history" | "clearphist" => queued ClearPromptHistory,
        "show-prompt-history" | "showphist" => queued ShowPromptHistory,
        ;
        "kill-server" => parse_no_args("kill-server", arguments).map(|()| Command::KillServer),
        "lock-server" => parse_no_args("lock-server", arguments).map(|()| Command::LockServer),
        "server-access" => parse_server_access_args(arguments).map(Command::ServerAccess),
        "rename-window" => parse_rename_window_args(arguments).map(Command::RenameWindow),
        "split-window" => parse_split_window_args(arguments).map(Command::SplitWindow),
        "resize-pane" => parse_resize_pane_args(arguments).map(Command::ResizePane),
        "with-session" => parse_command_args_with_policy(
            "with-session",
            arguments,
            PositionalOptionPolicy::InterspersedBeforeSeparator,
        )
        .and_then(|args| Validate::validate(args, "with-session"))
        .map(Command::WithSession),
        "send-keys" => parse_send_keys_args(arguments).map(Command::SendKeys),
        "set-option" => parse_set_option_args(SetOptionCommandKind::SetOption, arguments)
            .map(Command::SetOption),
        "set-window-option" => {
            parse_set_option_args(SetOptionCommandKind::SetWindowOption, arguments)
                .map(Command::SetWindowOption)
        }
        "show-window-options" => parse_command_args("show-window-options", arguments)
            .map(|args: ShowWindowOptionsArgs| Command::ShowWindowOptions(args.into())),
        "source-file" => parse_source_file_args(arguments).map(Command::SourceFile),
        "web-share" => parse_web_share_args(arguments).map(Command::WebShare),
        "capabilities" if arguments.iter().any(|arg| arg == "--help") => Err(clap::Error::raw(
            clap::error::ErrorKind::DisplayHelp,
            "usage: rmux capabilities [--human|--json]\n",
        )),
        _ => Ok(Command::Unsupported(UnsupportedCommandArgs {
            name,
            arguments,
        })),
    };

    parsed.map_err(|mut error| {
        error.insert(
            clap::error::ContextKind::Custom,
            clap::error::ContextValue::String(error_command_name),
        );
        error
    })
}

/// Flattens command arguments back to strings, re-rendering nested command lists for reparsing.
fn command_arguments_for_clap(arguments: &[CommandArgument]) -> Vec<String> {
    arguments
        .iter()
        .map(|argument| match argument {
            CommandArgument::String(value) => value.clone(),
            CommandArgument::Commands(_) => argument.to_tmux_reparse_string(),
        })
        .collect()
}

/// Rejects any argument for a command that takes none, while still honouring `--help`.
fn parse_no_args(command_name: &'static str, arguments: Vec<String>) -> Result<(), clap::Error> {
    clap::Command::new(command_name)
        .no_binary_name(true)
        .disable_help_flag(true)
        .arg(help_argument())
        .try_get_matches_from(arguments)
        .map(|_| ())
}

/// Parses `send-keys`, requiring `--` before the payload when any `--wait` flag is used.
fn parse_send_keys_args(arguments: Vec<String>) -> Result<SendKeysArgs, clap::Error> {
    const WAIT_VALUE_FLAGS: [&str; 4] = [
        "--wait",
        "--wait-text",
        "--wait-visible-text",
        "--wait-next-text",
    ];
    let has_wait = arguments.iter().any(|argument| {
        argument == "--wait-pane-exit"
            || WAIT_VALUE_FLAGS.iter().any(|flag| {
                argument
                    .strip_prefix(flag)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
            })
    });
    if has_wait && !arguments.iter().any(|argument| argument == "--") {
        return Err(value_error(
            "send-keys",
            "-- is required before payload when using --wait options",
        ));
    }
    parse_command_args::<SendKeysArgs>("send-keys", arguments)?.validate("send-keys")
}

/// Parses `server-access`, rejecting long flags and unknown short flags such as `-t`.
fn parse_server_access_args(arguments: Vec<String>) -> Result<ServerAccessArgs, clap::Error> {
    for argument in &arguments {
        if argument == "--" {
            break;
        }
        if argument == "--help" {
            continue;
        }
        if argument == "-" || argument.starts_with("--") {
            let flag = if argument == "-" { "-" } else { "--" };
            return Err(clap::Error::raw(
                clap::error::ErrorKind::UnknownArgument,
                format!("command server-access: invalid flag {flag}"),
            ));
        }
        let Some(flags) = argument.strip_prefix('-') else {
            continue;
        };
        if let Some(flag) = flags
            .chars()
            .find(|flag| !matches!(flag, 'a' | 'd' | 'l' | 'r' | 'w'))
        {
            return Err(unknown_flag_error("server-access", &format!("-{flag}")));
        }
    }

    parse_command_args::<ServerAccessArgs>("server-access", arguments)?.validate("server-access")
}

/// Parses `set-option` / `set-window-option`, resolving scope flags and a literal `--` value.
fn parse_set_option_args(
    kind: SetOptionCommandKind,
    mut arguments: Vec<String>,
) -> Result<SetOptionArgs, clap::Error> {
    let command_name = kind.command_name();
    let trailing_literal_separator = normalize_set_option_separator(command_name, &mut arguments)?;
    let mut args = match kind {
        SetOptionCommandKind::SetOption => {
            let explicit_scope = set_option_scope(&arguments);
            let mut args = parse_command_args::<SetOptionArgs>(command_name, arguments)?;
            apply_set_option_scope(&mut args, explicit_scope);
            args
        }
        SetOptionCommandKind::SetWindowOption => {
            parse_command_args::<SetWindowOptionArgs>(command_name, arguments)?.into()
        }
    };
    if trailing_literal_separator {
        if args.value.is_some() {
            return Err(too_many_arguments_error(command_name, 2));
        }
        args.value = Some("--".to_owned());
    }
    args.validate(kind)
}

/// The option scope an explicit `set-option` flag selects, in precedence order.
#[derive(Debug, Clone, Copy)]
enum SetOptionScopeFlag {
    Server,
    Window,
    Pane,
}

/// Which of the `-s`, `-w` and `-p` scope flags appeared on a `set-option` line.
#[derive(Debug, Default, Clone, Copy)]
struct SetOptionScopeFlags {
    server: bool,
    window: bool,
    pane: bool,
}

impl SetOptionScopeFlags {
    /// Picks the winning scope, with server beating pane and pane beating window.
    const fn selected(self) -> Option<SetOptionScopeFlag> {
        if self.server {
            Some(SetOptionScopeFlag::Server)
        } else if self.pane {
            Some(SetOptionScopeFlag::Pane)
        } else if self.window {
            Some(SetOptionScopeFlag::Window)
        } else {
            None
        }
    }
}

/// Scans `set-option` flag clusters for an explicit scope, skipping `-t` and its target value.
fn set_option_scope(arguments: &[String]) -> Option<SetOptionScopeFlag> {
    let mut scopes = SetOptionScopeFlags::default();
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        if argument == "--" {
            break;
        }
        let Some(flags) = argument.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            break;
        };
        if argument.starts_with("-t") && argument.len() > 2 {
            index += 1;
            continue;
        }

        let mut chars = flags.chars().peekable();
        while let Some(flag) = chars.next() {
            match flag {
                's' => scopes.server = true,
                // -U is an unset modifier, not a scope selector: plain
                // `set -U` targets the session copy (oracle 2026-07-09) and
                // -p/-w keep their own precedence when combined with it.
                'w' => scopes.window = true,
                'p' => scopes.pane = true,
                't' => {
                    if chars.peek().is_none() {
                        index += 1;
                    }
                    break;
                }
                _ => {}
            }
        }
        index += 1;
    }

    scopes.selected()
}

/// Forces exactly one scope field on the parsed arguments when a flag selected one.
const fn apply_set_option_scope(args: &mut SetOptionArgs, scope: Option<SetOptionScopeFlag>) {
    let Some(scope) = scope else {
        return;
    };
    args.server = false;
    args.window = false;
    args.pane = false;
    match scope {
        SetOptionScopeFlag::Server => args.server = true,
        SetOptionScopeFlag::Window => args.window = true,
        SetOptionScopeFlag::Pane => args.pane = true,
    }
}

/// Drops the `--` separator, reporting whether it trailed and so denotes a literal `--` value.
fn normalize_set_option_separator(
    command_name: &'static str,
    arguments: &mut Vec<String>,
) -> Result<bool, clap::Error> {
    let Some(index) = arguments.iter().position(|argument| argument == "--") else {
        return Ok(false);
    };
    if index + 1 == arguments.len() {
        let _ = arguments.pop();
        return Ok(true);
    }
    if set_option_positionals_before_separator(&arguments[..index]) > 0 {
        return Err(too_many_arguments_error(command_name, 2));
    }
    let _ = arguments.remove(index);
    Ok(false)
}

/// Counts positional words before a `--`, skipping flag clusters and any `-t` target value.
fn set_option_positionals_before_separator(arguments: &[String]) -> usize {
    let mut positionals = 0;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "-t" {
            index += 2;
            continue;
        }
        if argument.starts_with("-t") && argument.len() > 2 {
            index += 1;
            continue;
        }
        if let Some(flags) = argument.strip_prefix('-').filter(|flags| !flags.is_empty()) {
            if let Some((offset, flag)) = flags.char_indices().find(|(_, flag)| *flag == 't') {
                index += if offset + flag.len_utf8() == flags.len() {
                    2
                } else {
                    1
                };
                continue;
            }
            index += 1;
            continue;
        }
        positionals += 1;
        index += 1;
    }
    positionals
}
