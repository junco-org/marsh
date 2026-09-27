use std::path::Path;
use std::process::Command;
use std::time::Duration;

use rmux_proto::{KillSessionRequest, Response, SessionName};

use crate::cli_args::WithSessionArgs;

use super::super::ExitFailure;
use super::super::target_resolution::connect_cli;
use super::common::{duration_millis, response_error, sleep_poll_interval};

/// Runs `with-session`: holds a renewed session lease for as long as a child command runs.
pub(crate) fn run_with_session(
    args: WithSessionArgs,
    socket_path: &Path,
) -> Result<i32, ExitFailure> {
    if args.command.is_empty() {
        return Err(ExitFailure::new(
            1,
            "with-session requires a child command".to_owned(),
        ));
    }

    let ttl_millis = duration_millis(args.ttl);
    let mut connection = connect_cli(socket_path)?;
    let lease = create_lease(&mut connection, args.session_name.clone(), ttl_millis)?;
    let mut child = match Command::new(&args.command[0])
        .args(&args.command[1..])
        .env("RMUX_SESSION", args.session_name.as_str())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = release_lease(&mut connection, args.session_name.clone(), lease.token);
            return Err(ExitFailure::new(
                1,
                format!("with-session failed to spawn child: {error}"),
            ));
        }
    };

    let renew_interval = renew_interval(args.ttl);
    let mut time_to_renew = renew_interval;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if args.kill_on_owner_exit {
                    kill_owned_session(&mut connection, args.session_name)?;
                } else {
                    release_lease(&mut connection, args.session_name, lease.token)?;
                }
                return Ok(status.code().unwrap_or(1));
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = release_lease(&mut connection, args.session_name, lease.token);
                return Err(ExitFailure::new(
                    1,
                    format!("with-session failed while waiting for child: {error}"),
                ));
            }
        }

        if time_to_renew <= super::common::POLL_INTERVAL {
            if let Err(error) = renew_lease(
                &mut connection,
                args.session_name.clone(),
                lease.token,
                ttl_millis,
            ) {
                let _ = child.kill();
                let _ = release_lease(&mut connection, args.session_name, lease.token);
                return Err(error);
            }
            time_to_renew = renew_interval;
        } else {
            time_to_renew = time_to_renew.saturating_sub(super::common::POLL_INTERVAL);
        }
        sleep_poll_interval();
    }
}

/// A held session lease, identified by the token the daemon issued for it.
struct Lease {
    token: u64,
}

/// Takes a lease on the session, refusing to start the child when the daemon declines.
fn create_lease(
    connection: &mut rmux_client::Connection,
    session_name: SessionName,
    ttl_millis: u64,
) -> Result<Lease, ExitFailure> {
    match connection
        .create_session_lease(session_name, ttl_millis)
        .map_err(ExitFailure::from)?
    {
        Response::CreateSessionLease(response) => Ok(Lease {
            token: response.token,
        }),
        other => Err(response_error(&other, "with-session", "for with-session")),
    }
}

/// Extends the lease before it expires, failing when the daemon says the lease was lost.
fn renew_lease(
    connection: &mut rmux_client::Connection,
    session_name: SessionName,
    token: u64,
    ttl_millis: u64,
) -> Result<(), ExitFailure> {
    match connection
        .renew_session_lease(session_name, token, ttl_millis)
        .map_err(ExitFailure::from)?
    {
        Response::RenewSessionLease(response) if response.renewed => Ok(()),
        Response::RenewSessionLease(_) => Err(ExitFailure::new(1, "with-session lease was lost")),
        other => Err(response_error(&other, "with-session", "for with-session")),
    }
}

/// Gives the lease back so another owner can take the session.
fn release_lease(
    connection: &mut rmux_client::Connection,
    session_name: SessionName,
    token: u64,
) -> Result<(), ExitFailure> {
    match connection
        .release_session_lease(session_name, token)
        .map_err(ExitFailure::from)?
    {
        Response::ReleaseSessionLease(response) if response.released => Ok(()),
        Response::ReleaseSessionLease(_) => Err(ExitFailure::new(
            1,
            "with-session lease was already released or lost",
        )),
        other => Err(response_error(
            &other,
            "with-session",
            "for with-session release",
        )),
    }
}

/// Kills the leased session on owner exit, treating an already-gone session as success.
fn kill_owned_session(
    connection: &mut rmux_client::Connection,
    session_name: SessionName,
) -> Result<(), ExitFailure> {
    match connection
        .kill_session(KillSessionRequest {
            target: session_name,
            kill_all_except_target: false,
            clear_alerts: false,
            kill_group: false,
        })
        .map_err(ExitFailure::from)?
    {
        Response::KillSession(_) => Ok(()),
        Response::Error(error)
            if matches!(error.error, rmux_proto::RmuxError::SessionNotFound(_)) =>
        {
            Ok(())
        }
        other => Err(response_error(
            &other,
            "with-session",
            "for with-session cleanup",
        )),
    }
}

/// How often to renew: a third of the lease time-to-live, clamped to 100ms through 1s.
fn renew_interval(ttl: Duration) -> Duration {
    let third = ttl / 3;
    third.clamp(Duration::from_millis(100), Duration::from_secs(1))
}
