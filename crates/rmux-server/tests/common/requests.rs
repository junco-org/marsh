//! The request payloads [`Fixture`] and [`TestRequest`] cover.
//!
//! Each default is the value these tests spelled out most often for that field.

use rmux_proto::{
    BreakPaneRequest, BreakPaneResponse, CapturePaneRequest, CapturePaneResponse,
    DeleteBufferRequest, DeleteBufferResponse, DisplayMessageRequest, DisplayMessageResponse,
    HookLifecycle, HookName, IfShellRequest, IfShellResponse, JoinPaneRequest, JoinPaneResponse,
    KillPaneRequest, KillPaneResponse, KillSessionRequest, KillSessionResponse, ListPanesRequest,
    ListPanesResponse, ListSessionsRequest, ListSessionsResponse, ListWindowsRequest,
    ListWindowsResponse, LoadBufferRequest, LoadBufferResponse, NewSessionExtRequest,
    NewSessionRequest, NewSessionResponse, NewWindowRequest, NewWindowResponse, OptionName,
    PaneTarget, PasteBufferRequest, PasteBufferResponse, RenameSessionRequest,
    RenameSessionResponse, Request, Response, RunShellRequest, RunShellResponse, SaveBufferRequest,
    SaveBufferResponse, ScopeSelector, SelectPaneRequest, SelectPaneResponse, SelectWindowRequest,
    SelectWindowResponse, SendKeysRequest, SendKeysResponse, SessionName, SetBufferRequest,
    SetBufferResponse, SetEnvironmentRequest, SetEnvironmentResponse, SetHookRequest,
    SetHookResponse, SetOptionMode, SetOptionRequest, SetOptionResponse, SplitDirection,
    SplitWindowRequest, SplitWindowResponse, SplitWindowTarget, SuspendClientRequest,
    SuspendClientResponse, WaitForRequest, WaitForResponse, WindowTarget,
};

use super::{Fixture, Owned, TestRequest, DEFAULT_SIZE};

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

/// `kill-session` of just `target`.
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

/// Detached, unnamed `new-window` in `session` at the next free index.
impl<N: Owned<SessionName>> Fixture<N> for NewWindowRequest {
    fn fixture(session: N) -> Self {
        Self {
            target: session.owned(),
            name: None,
            detached: true,
            start_directory: None,
            environment: None,
            command: None,
            process_command: None,
            target_window_index: None,
            insert_at_target: false,
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

/// Plain `select-pane` of `target`.
impl<P: Owned<PaneTarget>> Fixture<P> for SelectPaneRequest {
    fn fixture(target: P) -> Self {
        Self {
            target: target.owned(),
            title: None,
            style: None,
            input_disabled: None,
            preserve_zoom: false,
        }
    }
}

/// `send-keys` of `keys` to `target`.
impl<P, K> Fixture<(P, K)> for SendKeysRequest
where
    P: Owned<PaneTarget>,
    K: IntoIterator,
    K::Item: Into<String>,
{
    fn fixture((target, keys): (P, K)) -> Self {
        Self {
            target: target.owned(),
            keys: keys.into_iter().map(Into::into).collect(),
        }
    }
}

/// Detached `join-pane` moving `source` vertically below `target`.
impl<S: Owned<PaneTarget>, T: Owned<PaneTarget>> Fixture<(S, T)> for JoinPaneRequest {
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

/// Detached `break-pane` of `source` into the unnamed window `target`.
impl<S: Owned<PaneTarget>, T: Owned<WindowTarget>> Fixture<(S, T)> for BreakPaneRequest {
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

/// `list-sessions -F format`, unfiltered and in default order.
impl<F: Into<String>> Fixture<F> for ListSessionsRequest {
    fn fixture(format: F) -> Self {
        Self {
            format: Some(format.into()),
            filter: None,
            sort_order: None,
            reversed: false,
        }
    }
}

/// `list-windows -F format` of `session`, unfiltered and in default order.
impl<N: Owned<SessionName>, F: Into<String>> Fixture<(N, F)> for ListWindowsRequest {
    fn fixture((session, format): (N, F)) -> Self {
        Self {
            target: session.owned(),
            format: Some(format.into()),
            filter: None,
            sort_order: None,
            reversed: false,
        }
    }
}

/// `list-panes -s -F format` of every window in `session`, unfiltered and in default order.
impl<N: Owned<SessionName>, F: Into<String>> Fixture<(N, F)> for ListPanesRequest {
    fn fixture((session, format): (N, F)) -> Self {
        Self {
            target: session.owned(),
            format: Some(format.into()),
            filter: None,
            sort_order: None,
            reversed: false,
            target_window_index: None,
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

/// `set-option` replacing `option` in `scope` with `value`.
impl<V: Into<String>> Fixture<(ScopeSelector, OptionName, V)> for SetOptionRequest {
    fn fixture((scope, option, value): (ScopeSelector, OptionName, V)) -> Self {
        Self {
            scope,
            option,
            value: value.into(),
            mode: SetOptionMode::Replace,
        }
    }
}

/// Persistent `set-hook` of `hook` to `command` in `scope`.
impl<C: Into<String>> Fixture<(ScopeSelector, HookName, C)> for SetHookRequest {
    fn fixture((scope, hook, command): (ScopeSelector, HookName, C)) -> Self {
        Self {
            scope,
            hook,
            command: command.into(),
            lifecycle: HookLifecycle::Persistent,
        }
    }
}

/// Plain `set-environment name value` in `scope`.
impl<'a> Fixture<(ScopeSelector, &'a str, &'a str)> for SetEnvironmentRequest {
    fn fixture((scope, name, value): (ScopeSelector, &'a str, &'a str)) -> Self {
        Self {
            scope,
            name: name.to_owned(),
            value: value.to_owned(),
            mode: None,
            hidden: false,
            format: false,
        }
    }
}

/// `set-buffer` pushing `content` as a new automatically named buffer.
impl<C: Into<Vec<u8>>> Fixture<C> for SetBufferRequest {
    fn fixture(content: C) -> Self {
        Self {
            name: None,
            content: content.into(),
            append: false,
            new_name: None,
            set_clipboard: false,
            target_client: None,
        }
    }
}

/// `paste-buffer` of the newest buffer into `target`, kept afterwards.
impl<P: Owned<PaneTarget>> Fixture<P> for PasteBufferRequest {
    fn fixture(target: P) -> Self {
        Self {
            name: None,
            target: target.owned(),
            delete_after: false,
            separator: None,
            linefeed: false,
            raw: false,
            bracketed: false,
        }
    }
}

/// `load-buffer -b name path`.
impl<'a, P: Into<String>> Fixture<(P, &'a str)> for LoadBufferRequest {
    fn fixture((path, name): (P, &'a str)) -> Self {
        Self {
            path: path.into(),
            cwd: None,
            name: Some(name.to_owned()),
            set_clipboard: false,
            target_client: None,
        }
    }
}

/// `save-buffer -b name path`, replacing the file.
impl<'a, P: Into<String>> Fixture<(P, &'a str)> for SaveBufferRequest {
    fn fixture((path, name): (P, &'a str)) -> Self {
        Self {
            path: path.into(),
            cwd: None,
            name: Some(name.to_owned()),
            append: false,
        }
    }
}

/// Foreground `run-shell command` with stderr discarded.
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

/// Foreground `if-shell -F condition then_command` without an else branch or target.
impl<C: Into<String>, T: Into<String>> Fixture<(C, T)> for IfShellRequest {
    fn fixture((condition, then_command): (C, T)) -> Self {
        Self {
            condition: condition.into(),
            format_mode: true,
            then_command: then_command.into(),
            else_command: None,
            target: None,
            caller_cwd: None,
            background: false,
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
    SelectWindowRequest => SelectWindow => SelectWindow(SelectWindowResponse);
    ListWindowsRequest => ListWindows(Box) => ListWindows(ListWindowsResponse);
    SplitWindowRequest => SplitWindow => SplitWindow(SplitWindowResponse);
    SelectPaneRequest => SelectPane(Box) => SelectPane(SelectPaneResponse);
    KillPaneRequest => KillPane => KillPane(KillPaneResponse);
    BreakPaneRequest => BreakPane(Box) => BreakPane(BreakPaneResponse);
    JoinPaneRequest => JoinPane => JoinPane(JoinPaneResponse);
    ListPanesRequest => ListPanes(Box) => ListPanes(ListPanesResponse);
    ListSessionsRequest => ListSessions => ListSessions(ListSessionsResponse);
    CapturePaneRequest => CapturePane(Box) => CapturePane(CapturePaneResponse);
    SendKeysRequest => SendKeys => SendKeys(SendKeysResponse);
    DisplayMessageRequest => DisplayMessage => DisplayMessage(DisplayMessageResponse);
    SetOptionRequest => SetOption => SetOption(SetOptionResponse);
    SetHookRequest => SetHook => SetHook(SetHookResponse);
    SetEnvironmentRequest => SetEnvironment(Box) => SetEnvironment(SetEnvironmentResponse);
    SetBufferRequest => SetBuffer(Box) => SetBuffer(SetBufferResponse);
    PasteBufferRequest => PasteBuffer(Box) => PasteBuffer(PasteBufferResponse);
    DeleteBufferRequest => DeleteBuffer => DeleteBuffer(DeleteBufferResponse);
    LoadBufferRequest => LoadBuffer(Box) => LoadBuffer(LoadBufferResponse);
    SaveBufferRequest => SaveBuffer => SaveBuffer(SaveBufferResponse);
    RunShellRequest => RunShell(Box) => RunShell(RunShellResponse);
    IfShellRequest => IfShell(Box) => IfShell(IfShellResponse);
    WaitForRequest => WaitFor => WaitFor(WaitForResponse);
    SuspendClientRequest => SuspendClient => SuspendClient(SuspendClientResponse);
}
