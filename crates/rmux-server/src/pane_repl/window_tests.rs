//! What `sd` and a trailing `&` actually produce: a window the user can see.
//!
//! The editing cases next door are pure functions of the bytes that arrived. These deliberately
//! are not. Plan line 369 asks the prompt's two creation forms to go through rmux's own
//! window-creation transaction, in the session the line was typed in, detached — and none of
//! those three claims can be observed without a real handler, a real engine and a real session.
//! `list-windows` is the observation, because it is what a user and a client both see.

use std::collections::HashSet;

use marsh_core::shellmux::{JobIo, JobView};
use rmux_proto::{ListWindowsRequest, SessionName, WindowListEntry};

use super::Prompt;
use crate::handler::RequestHandler;
use crate::io::ShellIo;

/// A handler with a real engine, one detached session, and a prompt over that session's pane.
///
/// The seed gets a `docs` directory because `sd api docs` names one, and the engine refuses a job
/// directory that does not exist in the seed.
async fn prompt_over_session(name: &str) -> (RequestHandler, ShellIo, SessionName, Prompt) {
    let handler = RequestHandler::new();
    let io = crate::managed_workload::test_engine::install(&handler).expect("a test engine");

    let seed = io.default_dir().to_path_buf();
    std::fs::create_dir_all(seed.join("docs")).expect("a directory for `sd` to name");

    let session = handler.create_session(name).await;

    let view = terminal_jobs(&io)
        .into_iter()
        .next()
        .expect("the session's initial pane has a job");
    let job = io.shell(&view.id).expect("the pane's job is live");
    let prompt = Prompt::new(io.unleased(), job);
    (handler, io, session, prompt)
}

/// Every job a user could be attached to, which is every job with a terminal.
fn terminal_jobs(io: &ShellIo) -> Vec<JobView> {
    io.jobs()
        .into_iter()
        .filter(|view| matches!(view.io, JobIo::Terminal { .. }))
        .collect()
}

async fn list_windows(handler: &RequestHandler, session: &SessionName) -> Vec<WindowListEntry> {
    handler
        .handle_ok(ListWindowsRequest {
            target: session.clone(),
            format: None,
            filter: None,
            sort_order: None,
            reversed: false,
        })
        .await
        .windows
}

/// Which window the session is showing.
fn selected_window(windows: &[WindowListEntry]) -> u32 {
    windows
        .iter()
        .find(|entry| entry.active)
        .expect("a session always shows one of its windows")
        .target
        .window_index()
}

#[tokio::test]
async fn sd_opens_a_visible_detached_window_over_the_directory_it_named() {
    let (handler, io, session, prompt) = prompt_over_session("repl-sd-window").await;
    let before = list_windows(&handler, &session).await;
    assert_eq!(before.len(), 1);
    let showing = selected_window(&before);

    let report = prompt
        .spawn_job(Some("api".to_owned()), Some("docs"), None)
        .await;
    assert_eq!(report, vec!["%api started".to_owned()]);

    let after = list_windows(&handler, &session).await;
    assert_eq!(
        after.len(),
        2,
        "`sd` must add a window to the session the line was typed in"
    );
    assert_eq!(
        selected_window(&after),
        showing,
        "`sd` adds a job; only `fg` switches to one"
    );
    let opened = after
        .iter()
        .find(|entry| entry.target.window_index() != showing)
        .expect("the window `sd` created");
    assert_eq!(
        opened.name.as_deref(),
        Some("api"),
        "a job the user named keeps that name on its window"
    );

    let job = terminal_jobs(&io)
        .into_iter()
        .find(|view| view.id.as_str() == "api")
        .expect("the window presents the job the prompt asked for");
    assert_eq!(
        job.sandbox.dir.as_str(),
        "docs",
        "the directory typed at the prompt must survive the profile round trip"
    );
}

#[tokio::test]
async fn a_trailing_ampersand_opens_a_detached_window_whose_job_keep_can_still_retain() {
    let (handler, io, session, prompt) = prompt_over_session("repl-bg-window").await;
    let before = list_windows(&handler, &session).await;
    let showing = selected_window(&before);
    let existing = terminal_jobs(&io)
        .iter()
        .map(|view| view.id.as_str().to_owned())
        .collect::<HashSet<_>>();

    let report = prompt.spawn_job(None, None, Some("sleep 30")).await;

    let mut opened = terminal_jobs(&io)
        .into_iter()
        .filter(|view| !existing.contains(view.id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(opened.len(), 1, "`&` opens exactly one job");
    let opened = opened.pop().expect("the job `&` opened");
    assert_eq!(report, vec![format!("{} started", opened.id.reference())]);

    let after = list_windows(&handler, &session).await;
    assert_eq!(
        after.len(),
        before.len() + 1,
        "`&` must add a window, not a surfaceless job"
    );
    assert_eq!(
        selected_window(&after),
        showing,
        "`&` adds a job; only `fg` switches to one"
    );

    let handle = io
        .shell(&opened.id)
        .expect("the background job is still live");
    assert!(
        io.keep(&handle).expect("keep answers for a live job"),
        "an anonymous `&` keeps the engine's automatic closure, and `keep` must still cancel it"
    );
}
