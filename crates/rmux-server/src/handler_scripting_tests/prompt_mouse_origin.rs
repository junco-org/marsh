use super::*;
use crate::test_fixtures::TestRequest;

#[derive(Clone, Copy)]
enum PromptCase {
    CommandForeground,
    CommandBackground,
    CommandIncremental,
    ConfirmForeground,
    ConfirmBackground,
}

impl PromptCase {
    const ALL: [Self; 5] = [
        Self::CommandForeground,
        Self::CommandBackground,
        Self::CommandIncremental,
        Self::ConfirmForeground,
        Self::ConfirmBackground,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::CommandForeground => "command-foreground",
            Self::CommandBackground => "command-background",
            Self::CommandIncremental => "command-incremental",
            Self::ConfirmForeground => "confirm-foreground",
            Self::ConfirmBackground => "confirm-background",
        }
    }

    fn is_detached(self) -> bool {
        matches!(
            self,
            Self::CommandBackground | Self::CommandIncremental | Self::ConfirmBackground
        )
    }

    fn command(self, action: &str) -> String {
        match self {
            Self::CommandForeground => format!("command-prompt -1 -pvalue {{ {action} }}"),
            Self::CommandBackground => format!("command-prompt -b -1 -pvalue {{ {action} }}"),
            Self::CommandIncremental => format!("command-prompt -i -pvalue {{ {action} }}"),
            Self::ConfirmForeground => format!("confirm-before -pconfirm {{ {action} }}"),
            Self::ConfirmBackground => format!("confirm-before -b -pconfirm {{ {action} }}"),
        }
    }

    fn response(self) -> &'static [u8] {
        match self {
            Self::CommandForeground | Self::CommandBackground => b"x",
            Self::CommandIncremental => b"\x1b",
            Self::ConfirmForeground | Self::ConfirmBackground => b"y",
        }
    }
}

async fn start_prompt(
    handler: &RequestHandler,
    case: PromptCase,
    command: &str,
    context: QueueExecutionContext,
) -> Option<tokio::task::JoinHandle<Result<(), RmuxError>>> {
    let parsed = CommandParser::new()
        .parse(command)
        .unwrap_or_else(|error| panic!("command {command:?} parses: {error}"));
    let execution_handler = handler.clone();
    let task = tokio::spawn(async move {
        execution_handler
            .execute_parsed_commands(std::process::id(), parsed, context)
            .await
            .map(|_| ())
    });

    tokio::time::timeout(background_shell_test_timeout(), async {
        loop {
            if handler
                .attached_prompt_render(std::process::id())
                .await
                .is_some()
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("prompt becomes active");

    if case.is_detached() {
        task.await
            .expect("background prompt task joins")
            .expect("background prompt starts");
        None
    } else {
        Some(task)
    }
}

async fn finish_prompt(
    handler: &RequestHandler,
    case: PromptCase,
    foreground: Option<tokio::task::JoinHandle<Result<(), RmuxError>>>,
) {
    handler
        .handle_attached_live_input_for_test(std::process::id(), case.response())
        .await
        .unwrap_or_else(|error| panic!("{} input succeeds: {error}", case.label()));
    if let Some(task) = foreground {
        task.await
            .expect("foreground prompt task joins")
            .unwrap_or_else(|error| panic!("{} executes: {error}", case.label()));
    } else if !matches!(case, PromptCase::CommandIncremental) {
        wait_for_background_task(
            handler,
            "rmux-prompt-finish",
            background_shell_test_timeout(),
        )
        .await;
    }
}

async fn prepare_copy_cursor(handler: &RequestHandler, target: &PaneTarget) {
    seed_copy_mode_screen(handler, target).await;
    let parsed = CommandParser::new()
        .parse(&copy_cursor_command(target))
        .expect("copy setup parses");
    handler
        .execute_parsed_commands_for_test(std::process::id(), parsed)
        .await
        .expect("copy setup executes");
}

#[tokio::test]
async fn detached_prompts_drop_mouse_event_but_foreground_prompts_preserve_it() {
    // Oracle tmux 3.7b: background and incremental prompt callbacks append a
    // command with a fresh cmdq state; foreground callbacks reuse the item state.
    for case in PromptCase::ALL {
        let name = format!("prompt-event-{}", case.label());
        let (handler, session, target) = mouse_fixture(&name).await;
        let _control_rx = handler.attach_client(std::process::id(), &session).await;
        prepare_copy_cursor(&handler, &target).await;
        let context = QueueExecutionContext::without_caller_cwd()
            .with_current_target(Some(Target::Pane(target.clone())))
            .with_mouse_event(Some(mouse_event(&target)));
        let foreground = start_prompt(
            &handler,
            case,
            &case.command("send-keys -X begin-selection"),
            context,
        )
        .await;
        if matches!(case, PromptCase::CommandIncremental) {
            wait_for_background_task(
                &handler,
                "rmux-prompt-dispatch",
                background_shell_test_timeout(),
            )
            .await;
        }
        finish_prompt(&handler, case, foreground).await;

        let expected = if case.is_detached() {
            Some((6, 0))
        } else {
            Some((1, 1))
        };
        assert_eq!(
            selection_coordinates(&handler, &session).await,
            expected,
            "{} mouse event semantics",
            case.label()
        );
    }
}

#[tokio::test]
async fn detached_prompts_drop_mouse_target_but_foreground_prompts_preserve_it() {
    // `=` is tmux's current mouse target. A detached prompt callback has no
    // mouse target and therefore cannot select the clicked pane.
    for case in PromptCase::ALL {
        let name = format!("prompt-target-{}", case.label());
        let (handler, session, current) = mouse_fixture(&name).await;
        let _control_rx = handler.attach_client(std::process::id(), &session).await;
        TestRequest::send_ok(
            &handler,
            SplitWindowRequest {
                direction: SplitDirection::Horizontal,
                ..Fixture::fixture(&session)
            },
        )
        .await;
        TestRequest::send_ok(&handler, SelectPaneRequest::fixture(&current)).await;

        let mouse_target = PaneTarget::with_window(session.clone(), 0, 1);
        let context = QueueExecutionContext::without_caller_cwd()
            .with_current_target(Some(Target::Pane(current)))
            .with_mouse_target(Some(Target::Pane(mouse_target)));
        let foreground =
            start_prompt(&handler, case, &case.command("select-pane -t ="), context).await;
        if matches!(case, PromptCase::CommandIncremental) {
            wait_for_background_task(
                &handler,
                "rmux-prompt-dispatch",
                background_shell_test_timeout(),
            )
            .await;
        }
        finish_prompt(&handler, case, foreground).await;

        let active = {
            let state = handler.state.lock().await;
            state
                .sessions
                .session(&session)
                .expect("session exists")
                .window()
                .active_pane_index()
        };
        assert_eq!(
            active,
            if case.is_detached() { 0 } else { 1 },
            "{} mouse target semantics",
            case.label()
        );
    }
}
