//! The tunnel provider process, as a managed pipe job.
//!
//! A provider — `cloudflared`, `ngrok`, an `ssh -R` wrapper — is *structured external argv* from a
//! preset: a program name and a list of arguments, neither of which is shell text. It runs as
//! [`ProcessCommand::Argv`] so marsh quotes each argument itself. Joining them into a command line
//! would let a preset argument containing a space, a quote, a newline or a glob character become
//! several words, or none.
//!
//! It also **streams**. Readiness is decided from the provider's first lines while it keeps
//! running, so the execution is taken apart with [`crate::io::Execution::into_parts`] rather than
//! collected: a collection only returns once the process has exited, which for a tunnel that is
//! working never happens.
//!
//! # Readiness is provisional, and does not pretend otherwise
//!
//! Everything readiness rests on — the scanned lines, the matched URL, the TCP probe against the
//! public endpoint — is provisional output from a job whose publication gate has not run. That is
//! sound for this purpose: the bytes are bytes the provider really wrote, and the socket is a
//! socket that really accepted a connection. It is emphatically **not** a claim that anything the
//! provider wrote to the filesystem was approved. The command's verdict answers that, separately,
//! and this module keeps it whole rather than flattening it into an exit status.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use marsh_core::shellmux::{CommandCompletion, CommandHandle, WaitError};
use regex::Regex;
use rmux_proto::{ProcessCommand, RmuxError};
use tokio::net::{lookup_host, TcpStream};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use super::output;
use super::preset::{ProcessOutput, TunnelPreset};
use crate::io::{ShellHandle, ShellIo};
use crate::managed_workload;
use crate::web::origin::validate_public_base_url;
use crate::web::settings::WebShareSettings;
use crate::web::tunnel::TunnelInfo;

const ERROR_LINE_LIMIT: usize = 8;
/// How long a provider is given to close its tunnel after a `SIGTERM` before it is forced.
const STOP_GRACE_PERIOD: Duration = Duration::from_secs(5);
const PUBLIC_ENDPOINT_INITIAL_PROBE_DELAY: Duration = Duration::from_secs(1);
const PUBLIC_ENDPOINT_RETRY_DELAY: Duration = Duration::from_secs(1);
const PUBLIC_ENDPOINT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// What the provider's final verdict resolved to, or why it could not.
type ProviderVerdict = Result<Arc<CommandCompletion>, WaitError>;

/// A running provider. Dropping it stops the provider.
#[derive(Debug)]
pub(crate) struct TunnelHandle {
    provider: String,
    stop_tx: Option<oneshot::Sender<()>>,
    _readers: [JoinHandle<()>; 2],
    _output_task: Option<JoinHandle<()>>,
    _lifecycle: JoinHandle<()>,
}

impl Drop for TunnelHandle {
    /// Asks the lifecycle task to stop the provider.
    ///
    /// Only the request happens here. Signalling, the grace period and the forced discard are all
    /// asynchronous and belong to the task that owns the job; a `Drop` that blocked on them would
    /// stall whatever thread released the web share.
    fn drop(&mut self) {
        debug!(provider = %self.provider, "stopping web-share tunnel provider");
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
    }
}

pub(super) async fn start(
    io: &ShellIo,
    preset: TunnelPreset,
    settings: &WebShareSettings,
) -> Result<TunnelInfo, RmuxError> {
    let regex = Regex::new(&preset.url_pattern).map_err(|error| {
        RmuxError::Server(format!(
            "web-share tunnel preset '{}' has an invalid url_pattern: {error}",
            preset.name
        ))
    })?;
    let ready_regex = preset
        .ready_pattern
        .as_deref()
        .map(Regex::new)
        .transpose()
        .map_err(|error| {
            RmuxError::Server(format!(
                "web-share tunnel preset '{}' has an invalid ready_pattern: {error}",
                preset.name
            ))
        })?;
    let program = expand(&preset.program, settings)?;
    let args = preset
        .args
        .iter()
        .map(|arg| expand(arg, settings))
        .collect::<Result<Vec<_>, _>>()?;

    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(program.clone());
    argv.extend(args);
    let environment = daemon_environment();
    let spec = managed_workload::spec(
        io,
        &provider_directory(io),
        environment
            .iter()
            .map(|(name, value)| (name.as_os_str(), value.as_os_str())),
        // A one-off, so an automatic name. Nothing refers to this job again, and minting a stable
        // principal per web share would fill the policy history with names never used twice.
        None,
        ProcessCommand::Argv(argv),
    )
    .map_err(|error| spawn_error(&preset, &program, &error))?;
    let parts = managed_workload::start(io, spec)
        .await
        .map_err(|error| spawn_error(&preset, &program, &error))?
        .into_parts();

    // Upstream handed the provider `Stdio::null()`. The managed equivalent is a real pipe closed
    // immediately: a provider that reads standard input sees end of file instead of blocking on a
    // descriptor nothing will ever write to.
    if let Err(error) = parts.stdin.close().await {
        // Nothing owns this job yet — no reader, no lifecycle task — so leaving it running would
        // leak a provider nothing could ever stop.
        let _ = io.stop(&parts.shell, true).await;
        return Err(managed_workload::io_error(error));
    }

    let runtime = io.runtime();
    let (line_tx, line_rx) = output::channel();
    let readers = [
        output::spawn_reader(
            &runtime,
            parts.stdout,
            line_tx.clone(),
            ProcessOutput::Stdout,
        ),
        output::spawn_reader(&runtime, parts.stderr, line_tx, ProcessOutput::Stderr),
    ];

    let (stop_tx, stop_rx) = oneshot::channel();
    let (exit_tx, exit_rx) = oneshot::channel();
    let lifecycle = runtime.spawn(supervise(
        io.clone(),
        preset.name.clone(),
        parts.shell,
        parts.command,
        stop_rx,
        exit_tx,
    ));
    let mut handle = Some(TunnelHandle {
        provider: preset.name.clone(),
        stop_tx: Some(stop_tx),
        _readers: readers,
        _output_task: None,
        _lifecycle: lifecycle,
    });
    let (public_url, line_rx) =
        match wait_for_url(&preset, &regex, ready_regex.as_ref(), line_rx, exit_rx).await {
            Ok(url) => url,
            Err(error) => {
                drop(handle.take());
                return Err(error);
            }
        };
    let output_task = spawn_output_drain(&runtime, preset.name.clone(), line_rx);
    if let Some(handle) = &mut handle {
        handle._output_task = Some(output_task);
    }
    if let Err(error) = wait_for_public_endpoint(&preset, &public_url).await {
        drop(handle.take());
        return Err(error);
    }
    info!(
        provider = %preset.name,
        public_url,
        "web_share_tunnel_ready"
    );
    Ok(TunnelInfo {
        handle: handle.expect("handle remains when tunnel starts"),
        provider: preset.name,
        public_url,
    })
}

/// Owns the provider's job: waits for its verdict, and ends it when the handle is dropped.
///
/// A provider that exits on its own needs nothing from here. Upstream had to walk the process
/// tree and terminate what the provider had left behind, because it owned that tree; a managed
/// pipe job closes after its one admitted command and the engine reclaims its processes with it.
/// That is why the process-tree controller is gone rather than reimplemented.
async fn supervise(
    io: ShellIo,
    provider: String,
    shell: ShellHandle,
    command: CommandHandle,
    stop_rx: oneshot::Receiver<()>,
    exit_tx: oneshot::Sender<ProviderVerdict>,
) {
    let verdict = tokio::select! {
        verdict = command.wait() => verdict,
        _ = stop_rx => end_provider(&io, &shell, &command).await,
    };
    // The readiness scan holds this channel while it is still waiting for a URL, and a provider
    // that dies first is reported through it. Once the tunnel is up the scan is gone and the
    // verdict's only remaining reader is the operator log, so it is reported there rather than
    // silently dropped.
    if let Err(verdict) = exit_tx.send(verdict) {
        report_verdict(&provider, &verdict);
    }
}

/// Ends the provider: a catchable `SIGTERM`, the grace period, then a forced, discarding stop.
///
/// This is upstream's sequence expressed through the facade. The signal is what lets a provider
/// close its tunnel registration cleanly instead of leaving a dangling public hostname, and the
/// grace period is the same five seconds it always had.
async fn end_provider(
    io: &ShellIo,
    shell: &ShellHandle,
    command: &CommandHandle,
) -> ProviderVerdict {
    if let Err(error) = io.signal(shell, marsh_core::Signal::Terminate) {
        debug!("web-share tunnel provider could not be signalled: {error}");
    }
    match timeout(STOP_GRACE_PERIOD, command.wait()).await {
        Ok(verdict) => verdict,
        Err(_) => {
            // Forced, so every process the provider started goes with it; discarding, so a
            // provider killed part-way through a write leaves nothing behind in the seed.
            if let Err(error) = io.stop(shell, true).await {
                debug!("web-share tunnel provider could not be stopped: {error}");
            }
            command.wait().await
        }
    }
}

/// Logs the provider's final verdict once nothing is waiting for it.
///
/// Both halves, never flattened into one status: "the provider exited nonzero" and "the provider
/// exited zero and its writes were refused" are different events with different remedies, and an
/// operator reading one line has to be able to tell them apart.
fn report_verdict(provider: &str, verdict: &ProviderVerdict) {
    match verdict {
        Ok(completion) if completion.is_published() => debug!(
            provider,
            exit_code = ?completion.exit_code(),
            "web-share tunnel provider ended"
        ),
        Ok(completion) => warn!(
            provider,
            exit_code = ?completion.exit_code(),
            "web-share tunnel provider ended without approval:\n{}",
            managed_workload::completion_report(completion).trim_end()
        ),
        Err(error) => warn!(
            provider,
            "web-share tunnel provider ended without a verdict: {error}"
        ),
    }
}

/// How a provider that ended before printing a URL ended.
///
/// The exit status and the publication verdict are independent, and the interesting failure is the
/// one where they disagree: a provider that exited zero and had its work refused did not fail to
/// run, it failed to be approved, and only one of those is worth reinstalling a program over.
fn ended_detail(completion: &CommandCompletion) -> String {
    let status = completion.exit_code().map_or_else(
        || "ended without an exit status".to_owned(),
        |code| format!("exited with status {code}"),
    );
    if completion.is_published() {
        return format!("{status} before printing a public URL");
    }
    format!(
        "{status} before printing a public URL, and its work was not approved:\n{}",
        managed_workload::completion_report(completion).trim_end()
    )
}

/// The environment a provider inherits.
///
/// The daemon's own, as it had when it was a plain child of this process: `HOME`, `PATH` and
/// whatever credential variable `cloudflared` or `ngrok` was configured with all live there. A
/// managed specification *replaces* the profile environment rather than extending it, so the
/// daemon's has to be passed explicitly — an empty one would start the provider with nothing.
fn daemon_environment() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

/// The directory a provider starts in.
///
/// This host's default, which is where the provider was inherited from before. A tunnel provider
/// dials the network and resolves no relative path of its own, so there is nothing here to choose
/// between seeds: whichever seed the default lies in is the one it opens on.
fn provider_directory(io: &ShellIo) -> PathBuf {
    io.default_dir().to_path_buf()
}

async fn wait_for_public_endpoint(preset: &TunnelPreset, url: &str) -> Result<(), RmuxError> {
    let (host, port) = public_endpoint(url).map_err(|error| {
        RmuxError::Server(format!(
            "web-share tunnel provider '{}' printed an invalid public URL '{}': {error}",
            preset.name, url
        ))
    })?;
    let wait = async {
        sleep(PUBLIC_ENDPOINT_INITIAL_PROBE_DELAY).await;
        loop {
            if endpoint_accepts_connections(&host, port).await.is_ok() {
                return Ok::<(), ()>(());
            }
            sleep(PUBLIC_ENDPOINT_RETRY_DELAY).await;
        }
    };
    match timeout(Duration::from_secs(preset.ready_timeout_secs), wait)
        .await
        .map_err(|_| {
            RmuxError::Server(format!(
                "web-share tunnel provider '{}' printed '{}' but it did not become reachable within {}s",
                preset.name, url, preset.ready_timeout_secs
            ))
        })? {
        Ok(()) => Ok(()),
        Err(()) => unreachable!("public endpoint wait loop never returns an inner error"),
    }
}

async fn endpoint_accepts_connections(host: &str, port: u16) -> io::Result<()> {
    let addrs = ordered_endpoint_addrs(lookup_host((host, port)).await?);
    let mut last_error = None;
    for addr in addrs {
        match timeout(PUBLIC_ENDPOINT_CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(_stream)) => return Ok(()),
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => {
                last_error = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "connection attempt timed out",
                ));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "public endpoint did not resolve to any address",
        )
    }))
}

fn ordered_endpoint_addrs(addrs: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    let mut ipv4 = Vec::new();
    let mut ipv6 = Vec::new();
    for addr in addrs {
        if addr.is_ipv4() {
            ipv4.push(addr);
        } else {
            ipv6.push(addr);
        }
    }
    ipv4.extend(ipv6);
    ipv4
}

fn public_endpoint(url: &str) -> io::Result<(String, u16)> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing scheme"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, raw_port))
            if !host.is_empty() && raw_port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let port = raw_port.parse::<u16>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid port: {error}"),
                )
            })?;
            (host, port)
        }
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid authority",
            ));
        }
        None => (authority, default_port(scheme)?),
    };
    if host.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "missing host"));
    }
    Ok((host.to_owned(), port))
}

fn default_port(scheme: &str) -> io::Result<u16> {
    match scheme {
        "http" => Ok(80),
        "https" => Ok(443),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported URL scheme",
        )),
    }
}

async fn wait_for_url(
    preset: &TunnelPreset,
    regex: &Regex,
    ready_regex: Option<&Regex>,
    mut lines: mpsc::Receiver<(ProcessOutput, String)>,
    mut exit_rx: oneshot::Receiver<ProviderVerdict>,
) -> Result<(String, mpsc::Receiver<(ProcessOutput, String)>), RmuxError> {
    let mut last_lines = VecDeque::new();
    let ready = async {
        let mut found_url = None;
        let mut provider_ready = ready_regex.is_none();
        loop {
            tokio::select! {
                line = lines.recv() => {
                    let Some((source, line)) = line else {
                        return Err(tunnel_error(preset, "ended before printing a public URL", &last_lines));
                    };
                    remember_line(&mut last_lines, &line);
                    if !preset.url_source.accepts(source) {
                        continue;
                    }
                    if let Some(ready_regex) = ready_regex {
                        provider_ready |= ready_regex.is_match(&line);
                    }
                    if let Some(found) = regex.find(&line) {
                        found_url = Some(validate_public_base_url(found.as_str())?);
                    }
                    if provider_ready {
                        if let Some(url) = found_url.take() {
                            return Ok((url, lines));
                        }
                    }
                }
                verdict = &mut exit_rx => {
                    let detail = match verdict {
                        Ok(Ok(completion)) => ended_detail(&completion),
                        Ok(Err(error)) => format!("failed while waiting for tunnel process: {error}"),
                        Err(_) => "ended before printing a public URL".to_owned(),
                    };
                    return Err(tunnel_error(preset, &detail, &last_lines));
                }
            }
        }
    };
    timeout(Duration::from_secs(preset.ready_timeout_secs), ready)
        .await
        .map_err(|_| tunnel_error(preset, "timed out waiting for a public URL", &last_lines))?
}

fn spawn_output_drain(
    runtime: &Handle,
    provider: String,
    mut lines: mpsc::Receiver<(ProcessOutput, String)>,
) -> JoinHandle<()> {
    runtime.spawn(async move {
        while let Some((source, line)) = lines.recv().await {
            debug!(
                provider,
                source = ?source,
                line,
                "web-share tunnel provider output"
            );
        }
    })
}

fn remember_line(lines: &mut VecDeque<String>, line: &str) {
    if lines.len() == ERROR_LINE_LIMIT {
        lines.pop_front();
    }
    lines.push_back(line.to_owned());
}

fn tunnel_error(preset: &TunnelPreset, detail: &str, lines: &VecDeque<String>) -> RmuxError {
    let mut message = format!("web-share tunnel provider '{}' {detail}", preset.name);
    if !lines.is_empty() {
        message.push_str(". Last output:\n");
        for line in lines {
            message.push_str("  ");
            message.push_str(line);
            message.push('\n');
        }
    }
    if let Some(hint) = preset.install_hint.as_deref() {
        message.push_str(". ");
        message.push_str(hint);
    }
    RmuxError::Server(message)
}

/// The diagnostic for a provider this daemon could not admit at all.
///
/// Upstream appended the preset's install hint here, because a missing program arrived as a
/// `NotFound` spawn error. It no longer can: admission fails for engine reasons — no seed, a
/// non-UTF-8 environment, a closed host — and a program that does not exist is now an ordinary
/// nonzero exit, where [`tunnel_error`] appends the hint.
fn spawn_error(preset: &TunnelPreset, program: &str, error: &RmuxError) -> RmuxError {
    RmuxError::Server(format!(
        "failed to start web-share tunnel provider '{}' with '{}': {error}",
        preset.name, program
    ))
}

fn expand(value: &str, settings: &WebShareSettings) -> Result<String, RmuxError> {
    let expanded = value
        .replace("{host}", &settings.host)
        .replace("{port}", &settings.port.to_string());
    if expanded.contains('{') || expanded.contains('}') {
        return Err(RmuxError::Server(format!(
            "web-share tunnel preset contains an unknown placeholder in '{value}'"
        )));
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    #[test]
    fn endpoint_probe_prefers_ipv4_before_ipv6() {
        let ipv6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 443);
        let ipv4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 443);

        let ordered = super::ordered_endpoint_addrs([ipv6, ipv4]);

        assert_eq!(ordered, vec![ipv4, ipv6]);
    }

    /// The provider's URL is read off its managed standard output, and dropping stops it.
    ///
    /// This is the whole start path end to end: argv admitted as a pipe job, its stdout streamed
    /// through the line reader, the URL matched, and the public endpoint probed. The probe target
    /// is a real listener, because a tunnel whose URL nothing answers on is not ready.
    #[tokio::test]
    async fn runner_extracts_public_url_from_managed_output() {
        use super::start;
        use crate::handler::RequestHandler;
        use crate::web::settings::WebShareSettings;
        use crate::web::tunnel::preset::{TunnelPreset, UrlSource};
        use tokio::net::TcpListener;

        let handler = RequestHandler::new();
        let Ok(io) = crate::managed_workload::handler_facade(&handler) else {
            return;
        };
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind readiness probe listener");
        let port = listener.local_addr().expect("listener addr").port();
        let _listener_task =
            tokio::spawn(
                async move { while let Ok((_stream, _addr)) = listener.accept().await {} },
            );
        let url = format!("http://127.0.0.1:{port}");
        let preset = TunnelPreset {
            name: "test".to_owned(),
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), format!("printf '%s\\n' {url}; sleep 30")],
            url_pattern: regex::escape(&url),
            ready_pattern: None,
            url_source: UrlSource::Stdout,
            ready_timeout_secs: 20,
            install_hint: None,
        };

        let info = start(&io, preset, &WebShareSettings::default())
            .await
            .expect("tunnel starts");

        assert_eq!(info.public_url, url);
        drop(info);
    }
}
