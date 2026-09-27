//! The request payloads [`Fixture`] and [`TestRequest`] cover.
//!
//! Each default is the value the existing tests spelled out most often for that field.

use rmux_core::PaneId;
use rmux_proto::{
    BindKeyRequest, BindKeyResponse, BreakPaneRequest, BreakPaneResponse, CapturePaneRequest,
    CapturePaneResponse, ClockModeRequest, ClockModeResponse, CopyModeRequest, CopyModeResponse,
    CreateWebShareRequest, DeleteBufferRequest, DeleteBufferResponse, DisplayMessageExtRequest,
    DisplayMessageRequest, DisplayMessageResponse, HookLifecycle, HookName, IfShellRequest,
    IfShellResponse, JoinPaneRequest, JoinPaneResponse, KillPaneRequest, KillPaneResponse,
    KillSessionRequest, KillSessionResponse, KillWindowRequest, KillWindowResponse,
    LinkWindowRequest, LinkWindowResponse, ListPanesRequest, ListPanesResponse, ListWindowsRequest,
    ListWindowsResponse, MovePaneRequest, MovePaneResponse, MoveWindowRequest, MoveWindowResponse,
    MoveWindowTarget, NewSessionExtRequest, NewSessionRequest, NewSessionResponse,
    NewWindowRequest, NewWindowResponse, OptionName, OptionScopeSelector,
    PaneOutputSubscriptionStart, PaneStreamMode, PaneTarget, PaneTargetRef, PipePaneRequest,
    PipePaneResponse, RefreshClientRequest, RefreshClientResponse, RenameSessionRequest,
    RenameSessionResponse, RenameWindowRequest, RenameWindowResponse, Request, ResizePaneRequest,
    ResizePaneResponse, ResizeWindowRequest, ResizeWindowResponse, RespawnPaneRequest,
    RespawnPaneResponse, RespawnWindowRequest, RespawnWindowResponse, Response,
    RotateWindowRequest, RotateWindowResponse, RunShellRequest, RunShellResponse, ScopeSelector,
    SelectPaneRequest, SelectPaneResponse, SelectWindowRequest, SelectWindowResponse,
    SendKeysExtRequest, SendKeysRequest, SendKeysResponse, SessionName, SetBufferRequest,
    SetBufferResponse, SetEnvironmentRequest, SetEnvironmentResponse, SetHookMutationRequest,
    SetHookRequest, SetHookResponse, SetOptionByNameRequest, SetOptionByNameResponse,
    SetOptionMode, SetOptionRequest, SetOptionResponse, SourceFileRequest, SourceFileResponse,
    SplitDirection, SplitWindowExtRequest, SplitWindowRequest, SplitWindowResponse,
    SplitWindowTarget, SubscribePaneOutputRefRequest, SubscribePaneStateRequest,
    SubscribePaneStreamRequest, SwapPaneRequest, SwapPaneResponse, SwapWindowRequest,
    SwapWindowResponse, SwitchClientRequest, SwitchClientResponse, TerminalSize, UnbindKeyRequest,
    UnbindKeyResponse, UnlinkWindowRequest, UnlinkWindowResponse, WaitForMode, WaitForRequest,
    WaitForResponse, WebShareRequest, WebShareResponse, WebShareScope, WindowTarget,
};
use tokio::sync::mpsc;

use super::{Fixture, Owned, TestRequest};
use crate::handler::attach_support::{AttachRegistration, ClientFlags};
use crate::outer_terminal::OuterTerminalContext;
use crate::pane_io::{AttachControl, PaneAlertEvent};

/// The pane geometry every sized fixture starts from.
const DEFAULT_SIZE: TerminalSize = TerminalSize { cols: 80, rows: 24 };

/// Detached 80x24 `new-session` for `name`.
impl<N: Owned<SessionName>> Fixture<N> for NewSessionRequest {
    fn fixture(name: N) -> Self {
        Self {
            session_name: name.owned(),
            detached: true,
            size: Some(DEFAULT_SIZE),
            environment: None,
        }
    }
}

/// Detached 80x24 extended `new-session` for `name`, every option off.
impl<N: Owned<SessionName>> Fixture<N> for NewSessionExtRequest {
    fn fixture(name: N) -> Self {
        Self {
            session_name: Some(name.owned()),
            working_directory: None,
            detached: true,
            size: Some(DEFAULT_SIZE),
            environment: None,
            group_target: None,
            attach_if_exists: false,
            detach_other_clients: false,
            kill_other_clients: false,
            flags: None,
            window_name: None,
            print_session_info: false,
            print_format: None,
            command: None,
            process_command: None,
            client_environment: None,
            skip_environment_update: false,
        }
    }
}

/// Detached, unnamed `new-window` in `session` at the next free index.
impl<N: Owned<SessionName>> Fixture<N> for NewWindowRequest {
    fn fixture(session: N) -> Self {
        Self {
            target: session.owned(),
            name: None,
            detached: true,
            environment: None,
            command: None,
            start_directory: None,
            target_window_index: None,
            insert_at_target: false,
            process_command: None,
        }
    }
}

/// Vertical `split-window` of `target`: a session's active pane, or a pane.
impl<T: Owned<SplitWindowTarget>> Fixture<T> for SplitWindowRequest {
    fn fixture(target: T) -> Self {
        Self {
            target: target.owned(),
            direction: SplitDirection::Vertical,
            before: false,
            environment: None,
        }
    }
}

/// Vertical extended `split-window` of `target` that selects the new pane.
impl<T: Owned<SplitWindowTarget>> Fixture<T> for SplitWindowExtRequest {
    fn fixture(target: T) -> Self {
        Self {
            target: target.owned(),
            direction: SplitDirection::Vertical,
            before: false,
            environment: None,
            command: None,
            process_command: None,
            start_directory: None,
            keep_alive_on_exit: None,
            detached: false,
            size: None,
            preserve_zoom: false,
            full_size: false,
            stdin_payload: None,
        }
    }
}

/// Plain `copy-mode` on `target`.
impl<P: Owned<PaneTarget>> Fixture<P> for CopyModeRequest {
    fn fixture(target: P) -> Self {
        Self {
            target: Some(target.owned()),
            page_down: false,
            exit_on_scroll: false,
            hide_position: false,
            mouse_drag_start: false,
            cancel_mode: false,
            scrollbar_scroll: false,
            source: None,
            page_up: false,
        }
    }
}

/// `capture-pane -p` of `target`'s visible screen, trimmed and unescaped.
impl<P: Owned<PaneTarget>> Fixture<P> for CapturePaneRequest {
    fn fixture(target: P) -> Self {
        Self {
            target: target.owned(),
            start: None,
            end: None,
            print: true,
            buffer_name: None,
            alternate: false,
            escape_ansi: false,
            escape_sequences: false,
            include_format: false,
            hyperlinks: false,
            line_numbers: false,
            join_wrapped: false,
            use_mode_screen: false,
            preserve_trailing_spaces: false,
            do_not_trim_spaces: false,
            pending_input: false,
            quiet: false,
            start_is_absolute: false,
            end_is_absolute: false,
        }
    }
}

/// `send-keys` of `keys` to `target`, dispatched through the key table.
impl<P, K> Fixture<(P, K)> for SendKeysExtRequest
where
    P: Owned<PaneTarget>,
    K: IntoIterator,
    K::Item: Into<String>,
{
    fn fixture((target, keys): (P, K)) -> Self {
        Self {
            target: Some(target.owned()),
            keys: keys.into_iter().map(Into::into).collect(),
            expand_formats: false,
            hex: false,
            literal: false,
            dispatch_key_table: true,
            copy_mode_command: false,
            forward_mouse_event: false,
            reset_terminal: false,
            repeat_count: None,
        }
    }
}

/// `respawn-pane -k` of `target` with its original command, directory and environment.
impl<P: Owned<PaneTarget>> Fixture<P> for RespawnPaneRequest {
    fn fixture(target: P) -> Self {
        Self {
            target: target.owned(),
            kill: true,
            start_directory: None,
            environment: None,
            command: None,
            process_command: None,
        }
    }
}

/// Plain `select-pane` of `target`.
impl<P: Owned<PaneTarget>> Fixture<P> for SelectPaneRequest {
    fn fixture(target: P) -> Self {
        Self {
            target: target.owned(),
            title: None,
            input_disabled: None,
            preserve_zoom: false,
            style: None,
        }
    }
}

/// `display-message -p message` without an explicit target.
impl<M: Into<String>> Fixture<M> for DisplayMessageRequest {
    fn fixture(message: M) -> Self {
        Self {
            target: None,
            print: true,
            message: Some(message.into()),
            empty_target_context: false,
        }
    }
}

/// Extended `display-message message` shown on the status line, without an explicit target or
/// client.
impl<M: Into<String>> Fixture<M> for DisplayMessageExtRequest {
    fn fixture(message: M) -> Self {
        Self {
            target: None,
            print: false,
            message: Some(message.into()),
            target_client: None,
            empty_target_context: false,
            duration_ms: None,
            ignore_input: false,
        }
    }
}

/// `set-option` replacing `option` in `scope` with `value`.
impl<'a> Fixture<(ScopeSelector, OptionName, &'a str)> for SetOptionRequest {
    fn fixture((scope, option, value): (ScopeSelector, OptionName, &'a str)) -> Self {
        Self {
            scope,
            option,
            value: value.to_owned(),
            mode: SetOptionMode::Replace,
        }
    }
}

/// `set-option` replacing the option spelled `name` in `scope` with `value`.
impl<'a> Fixture<(OptionScopeSelector, &'a str, &'a str)> for SetOptionByNameRequest {
    fn fixture((scope, name, value): (OptionScopeSelector, &'a str, &'a str)) -> Self {
        Self {
            scope,
            name: name.to_owned(),
            value: Some(value.to_owned()),
            mode: SetOptionMode::Replace,
            only_if_unset: false,
            unset: false,
            unset_pane_overrides: false,
            format: false,
            format_target: None,
        }
    }
}

/// Persistent `set-hook` of `hook` to `command` in `scope`.
impl<'a> Fixture<(ScopeSelector, HookName, &'a str)> for SetHookRequest {
    fn fixture((scope, hook, command): (ScopeSelector, HookName, &'a str)) -> Self {
        Self {
            scope,
            hook,
            command: command.to_owned(),
            lifecycle: HookLifecycle::Persistent,
        }
    }
}

/// Persistent extended `set-hook` replacing `hook` with `command` in `scope`.
impl<'a> Fixture<(ScopeSelector, HookName, &'a str)> for SetHookMutationRequest {
    fn fixture((scope, hook, command): (ScopeSelector, HookName, &'a str)) -> Self {
        Self {
            scope,
            hook,
            command: Some(command.to_owned()),
            lifecycle: HookLifecycle::Persistent,
            append: false,
            unset: false,
            run_immediately: false,
            index: None,
        }
    }
}

/// Detached `link-window` of `source` into the free slot `target`.
impl<S, T> Fixture<(S, T)> for LinkWindowRequest
where
    S: Owned<WindowTarget>,
    T: Owned<WindowTarget>,
{
    fn fixture((source, target): (S, T)) -> Self {
        Self {
            source: source.owned(),
            target: target.owned(),
            after: false,
            before: false,
            kill_destination: false,
            detached: true,
        }
    }
}

/// Detached `move-window` of `source` into the free slot or session `target`.
impl<S, T> Fixture<(S, T)> for MoveWindowRequest
where
    S: Owned<WindowTarget>,
    T: Owned<MoveWindowTarget>,
{
    fn fixture((source, target): (S, T)) -> Self {
        Self {
            source: Some(source.owned()),
            target: target.owned(),
            renumber: false,
            kill_destination: false,
            detached: true,
            after: false,
            before: false,
        }
    }
}

/// Plain `refresh-client` of `target_client` (`None` for the requester), with no adjustment,
/// pan, flags or subscriptions.
impl Fixture<Option<String>> for RefreshClientRequest {
    fn fixture(target_client: Option<String>) -> Self {
        Self {
            target_client,
            adjustment: None,
            clear_pan: false,
            pan_left: false,
            pan_right: false,
            pan_up: false,
            pan_down: false,
            status_only: false,
            clipboard_query: false,
            flags: None,
            flags_alias: None,
            subscriptions: Vec::new(),
            subscriptions_format: Vec::new(),
            control_size: None,
            colour_report: None,
        }
    }
}

/// `kill-window` of `target` alone.
impl<W: Owned<WindowTarget>> Fixture<W> for KillWindowRequest {
    fn fixture(target: W) -> Self {
        Self {
            target: target.owned(),
            kill_all_others: false,
        }
    }
}

/// `wait-for` in `mode` on `channel`.
impl<C: Into<String>> Fixture<(C, WaitForMode)> for WaitForRequest {
    fn fixture((channel, mode): (C, WaitForMode)) -> Self {
        Self {
            channel: channel.into(),
            mode,
        }
    }
}

/// Detached `break-pane` of `source` into a new window at the next free slot.
impl<S: Owned<PaneTarget>> Fixture<S> for BreakPaneRequest {
    fn fixture(source: S) -> Self {
        Self {
            source: source.owned(),
            target: None,
            name: None,
            detached: true,
            after: false,
            before: false,
            print_target: false,
            format: None,
        }
    }
}

/// `kill-session` of `target` alone.
impl<N: Owned<SessionName>> Fixture<N> for KillSessionRequest {
    fn fixture(target: N) -> Self {
        Self {
            target: target.owned(),
            kill_all_except_target: false,
            clear_alerts: false,
            kill_group: false,
        }
    }
}

/// Detached `break-pane` of `source` into the window slot `target`, unnamed and unprinted.
impl<S, T> Fixture<(S, T)> for BreakPaneRequest
where
    S: Owned<PaneTarget>,
    T: Owned<WindowTarget>,
{
    fn fixture((source, target): (S, T)) -> Self {
        Self {
            source: source.owned(),
            target: Some(target.owned()),
            name: None,
            detached: true,
            after: false,
            before: false,
            print_target: false,
            format: None,
        }
    }
}

/// Detached vertical `join-pane` of `source` after `target`.
impl<S, T> Fixture<(S, T)> for JoinPaneRequest
where
    S: Owned<PaneTarget>,
    T: Owned<PaneTarget>,
{
    fn fixture((source, target): (S, T)) -> Self {
        Self {
            source: source.owned(),
            target: target.owned(),
            direction: SplitDirection::Vertical,
            detached: true,
            before: false,
            full_size: false,
            size: None,
        }
    }
}

/// Detached vertical `move-pane` of `source` after `target`.
impl<S, T> Fixture<(S, T)> for MovePaneRequest
where
    S: Owned<PaneTarget>,
    T: Owned<PaneTarget>,
{
    fn fixture((source, target): (S, T)) -> Self {
        Self {
            source: source.owned(),
            target: target.owned(),
            direction: SplitDirection::Vertical,
            detached: true,
            before: false,
            full_size: false,
            size: None,
        }
    }
}

/// Detached `swap-pane` of `source` and `target`.
impl<S, T> Fixture<(S, T)> for SwapPaneRequest
where
    S: Owned<PaneTarget>,
    T: Owned<PaneTarget>,
{
    fn fixture((source, target): (S, T)) -> Self {
        Self {
            source: source.owned(),
            target: target.owned(),
            direction: None,
            detached: true,
            preserve_zoom: false,
        }
    }
}

/// `source-file` of `paths`, loud, executed and unexpanded.
impl<P> Fixture<P> for SourceFileRequest
where
    P: IntoIterator,
    P::Item: Into<String>,
{
    fn fixture(paths: P) -> Self {
        Self {
            paths: paths.into_iter().map(Into::into).collect(),
            quiet: false,
            parse_only: false,
            verbose: false,
            expand_paths: false,
            target: None,
            caller_cwd: None,
            stdin: None,
        }
    }
}

/// Foreground `run-shell command` without arguments, delay or target.
impl<C: Into<String>> Fixture<C> for RunShellRequest {
    fn fixture(command: C) -> Self {
        Self {
            command: command.into(),
            arguments: Vec::new(),
            background: false,
            as_commands: false,
            show_stderr: false,
            delay_seconds: None,
            start_directory: None,
            target: None,
            source_depth: None,
        }
    }
}

/// Foreground `if-shell condition then_command` with no else branch or target.
impl<C: Into<String>, T: Into<String>> Fixture<(C, T)> for IfShellRequest {
    fn fixture((condition, then_command): (C, T)) -> Self {
        Self {
            condition: condition.into(),
            format_mode: false,
            then_command: then_command.into(),
            else_command: None,
            target: None,
            caller_cwd: None,
            background: false,
        }
    }
}

/// Non-repeating `bind-key -T table key command…` without a note.
impl<T, K, C> Fixture<(T, K, C)> for BindKeyRequest
where
    T: Into<String>,
    K: Into<String>,
    C: IntoIterator,
    C::Item: Into<String>,
{
    fn fixture((table, key, command): (T, K, C)) -> Self {
        Self {
            table_name: table.into(),
            key: key.into(),
            note: None,
            repeat: false,
            command: Some(command.into_iter().map(Into::into).collect()),
        }
    }
}

/// `set-buffer -b name content`, replacing the buffer without touching the clipboard.
impl<N: Into<String>, C: Into<Vec<u8>>> Fixture<(N, C)> for SetBufferRequest {
    fn fixture((name, content): (N, C)) -> Self {
        Self {
            name: Some(name.into()),
            content: content.into(),
            append: false,
            new_name: None,
            set_clipboard: false,
            target_client: None,
        }
    }
}

/// Pane-state subscription to the pane in slot `target`, including nothing optional.
impl<P: Owned<PaneTarget>> Fixture<P> for SubscribePaneStateRequest {
    fn fixture(target: P) -> Self {
        Self {
            target: PaneTargetRef::slot(target.owned()),
            include_title: false,
            include_options: false,
            include_foreground: false,
        }
    }
}

/// `mode` pane-stream subscription to the pane in slot `target`, without a snapshot.
impl<P: Owned<PaneTarget>> Fixture<(P, PaneStreamMode)> for SubscribePaneStreamRequest {
    fn fixture((target, mode): (P, PaneStreamMode)) -> Self {
        Self {
            target: PaneTargetRef::slot(target.owned()),
            mode,
            include_snapshot: false,
        }
    }
}

/// Pane-output subscription to pane `pane_id` of `session`, starting from now.
impl<N: Owned<SessionName>> Fixture<(N, rmux_proto::PaneId)> for SubscribePaneOutputRefRequest {
    fn fixture((session, pane_id): (N, rmux_proto::PaneId)) -> Self {
        Self {
            target: PaneTargetRef::by_id(session.owned(), pane_id),
            start: PaneOutputSubscriptionStart::Now,
        }
    }
}

/// A spectator-only web share of `scope` without a pin, expiry or public URL.
impl Fixture<WebShareScope> for CreateWebShareRequest {
    fn fixture(scope: WebShareScope) -> Self {
        Self {
            scope,
            public_base_url: None,
            tunnel_provider: None,
            frontend_url: None,
            ttl_seconds: None,
            expires_at_unix: None,
            max_spectators: None,
            max_operators: None,
            url_options: Default::default(),
            require_pin: false,
            operator_pin: None,
            spectator_pin: None,
            terminal_palette: None,
            operator: false,
            spectator: true,
            controls: false,
            kill_session_on_expire: false,
        }
    }
}

/// An alert batch from `pane_id` in `session` that reports nothing.
impl<N: Owned<SessionName>> Fixture<(N, PaneId)> for PaneAlertEvent {
    fn fixture((session, pane_id): (N, PaneId)) -> Self {
        Self {
            session_name: session.owned(),
            pane_id,
            bell_count: 0,
            title_changed: false,
            title_change: None,
            path_changed: false,
            clipboard_set: false,
            clipboard_writes: Vec::new(),
            clipboard_queries: Vec::new(),
            mouse_mode_changed: false,
            alternate_mode_changed: false,
            queue_activity_alert: false,
            generation: None,
        }
    }
}

/// A writable 80x24 attach of user `uid` whose controls go to `control_tx`.
impl Fixture<(mpsc::UnboundedSender<AttachControl>, u32)> for AttachRegistration {
    fn fixture((control_tx, uid): (mpsc::UnboundedSender<AttachControl>, u32)) -> Self {
        Self {
            control_tx,
            control_backlog: Default::default(),
            closing: Default::default(),
            persistent_overlay_epoch: Default::default(),
            terminal_context: OuterTerminalContext::default(),
            client_title: None,
            flags: ClientFlags::default(),
            render_stream: false,
            uid,
            user: rmux_os::identity::UserIdentity::Uid(uid),
            can_write: true,
            client_size: Some(DEFAULT_SIZE),
        }
    }
}

/// Implements [`TestRequest`] for each `payload => Request variant => Response variant(success)`
/// row; `(Box)` marks a request variant that boxes its payload.
macro_rules! test_requests {
    ($($payload:ty => $request:ident $(($boxed:ident))? => $response:ident($success:ty);)*) => {$(
        impl TestRequest for $payload {
            type Success = $success;

            fn into_request(self) -> Request {
                let payload = self;
                $(let payload = $boxed::new(payload);)?
                Request::$request(payload)
            }

            fn success(response: Response) -> Result<$success, Response> {
                match response {
                    Response::$response(success) => Ok(success),
                    other => Err(other),
                }
            }
        }
    )*};
}

test_requests! {
    NewSessionRequest => NewSession => NewSession(NewSessionResponse);
    NewSessionExtRequest => NewSessionExt(Box) => NewSession(NewSessionResponse);
    KillSessionRequest => KillSession => KillSession(KillSessionResponse);
    RenameSessionRequest => RenameSession => RenameSession(RenameSessionResponse);
    NewWindowRequest => NewWindow(Box) => NewWindow(NewWindowResponse);
    KillWindowRequest => KillWindow => KillWindow(KillWindowResponse);
    SelectWindowRequest => SelectWindow => SelectWindow(SelectWindowResponse);
    RenameWindowRequest => RenameWindow => RenameWindow(RenameWindowResponse);
    LinkWindowRequest => LinkWindow => LinkWindow(LinkWindowResponse);
    MoveWindowRequest => MoveWindow => MoveWindow(MoveWindowResponse);
    SwapWindowRequest => SwapWindow => SwapWindow(SwapWindowResponse);
    RotateWindowRequest => RotateWindow => RotateWindow(RotateWindowResponse);
    ResizeWindowRequest => ResizeWindow => ResizeWindow(ResizeWindowResponse);
    RespawnWindowRequest => RespawnWindow(Box) => RespawnWindow(RespawnWindowResponse);
    UnlinkWindowRequest => UnlinkWindow => UnlinkWindow(UnlinkWindowResponse);
    ListWindowsRequest => ListWindows(Box) => ListWindows(ListWindowsResponse);
    SplitWindowRequest => SplitWindow => SplitWindow(SplitWindowResponse);
    SplitWindowExtRequest => SplitWindowExt(Box) => SplitWindow(SplitWindowResponse);
    SelectPaneRequest => SelectPane(Box) => SelectPane(SelectPaneResponse);
    KillPaneRequest => KillPane => KillPane(KillPaneResponse);
    RespawnPaneRequest => RespawnPane(Box) => RespawnPane(RespawnPaneResponse);
    ResizePaneRequest => ResizePane => ResizePane(ResizePaneResponse);
    BreakPaneRequest => BreakPane(Box) => BreakPane(BreakPaneResponse);
    JoinPaneRequest => JoinPane => JoinPane(JoinPaneResponse);
    MovePaneRequest => MovePane => MovePane(MovePaneResponse);
    SwapPaneRequest => SwapPane => SwapPane(SwapPaneResponse);
    PipePaneRequest => PipePane => PipePane(PipePaneResponse);
    ListPanesRequest => ListPanes(Box) => ListPanes(ListPanesResponse);
    CapturePaneRequest => CapturePane(Box) => CapturePane(CapturePaneResponse);
    SendKeysRequest => SendKeys => SendKeys(SendKeysResponse);
    SendKeysExtRequest => SendKeysExt => SendKeys(SendKeysResponse);
    CopyModeRequest => CopyMode => CopyMode(CopyModeResponse);
    ClockModeRequest => ClockMode => ClockMode(ClockModeResponse);
    SwitchClientRequest => SwitchClient => SwitchClient(SwitchClientResponse);
    DisplayMessageRequest => DisplayMessage => DisplayMessage(DisplayMessageResponse);
    DisplayMessageExtRequest => DisplayMessageExt(Box) => DisplayMessage(DisplayMessageResponse);
    SetOptionRequest => SetOption => SetOption(SetOptionResponse);
    SetOptionByNameRequest => SetOptionByName(Box) => SetOptionByName(SetOptionByNameResponse);
    SetHookRequest => SetHook => SetHook(SetHookResponse);
    SetHookMutationRequest => SetHookMutation => SetHook(SetHookResponse);
    SetEnvironmentRequest => SetEnvironment(Box) => SetEnvironment(SetEnvironmentResponse);
    SourceFileRequest => SourceFile(Box) => SourceFile(SourceFileResponse);
    RunShellRequest => RunShell(Box) => RunShell(RunShellResponse);
    IfShellRequest => IfShell(Box) => IfShell(IfShellResponse);
    BindKeyRequest => BindKey(Box) => BindKey(BindKeyResponse);
    UnbindKeyRequest => UnbindKey => UnbindKey(UnbindKeyResponse);
    SetBufferRequest => SetBuffer(Box) => SetBuffer(SetBufferResponse);
    DeleteBufferRequest => DeleteBuffer => DeleteBuffer(DeleteBufferResponse);
    WaitForRequest => WaitFor => WaitFor(WaitForResponse);
    RefreshClientRequest => RefreshClient(Box) => RefreshClient(RefreshClientResponse);
    WebShareRequest => WebShare(Box) => WebShare(Box<WebShareResponse>);
}
