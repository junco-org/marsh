//! Linux cgroup v2 integration tests. Run under `systemd-run --user --scope -p Delegate=yes`.
#![cfg(target_os = "linux")]
#![cfg(test)]
#![allow(clippy::panic_in_result_fn)]

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};
use brush_core::builtins::{BoxFuture, ContentOptions, ContentType, Registration};
use brush_core::{
    CommandArg, CpuLimit, ExecutionContext, ExecutionResult, MIB, MemoryLimit, PipelineOptions,
    ResourceEvent, ResourceLimits, ResourceMonitorConfig, ResourceSample, Shell, SourceInfo,
};

// brush-core deliberately registers no builtins by default; install one real builtin to
// distinguish builtin-only execution from an external /bin/echo.
fn echo(
    context: ExecutionContext<'_>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<ExecutionResult, brush_core::Error>> {
    Box::pin(async move {
        let text = args
            .iter()
            .skip(1)
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        writeln!(context.stdout(), "{text}")?;
        Ok(ExecutionResult::success())
    })
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "builtin registration requires this signature"
)]
fn echo_help(
    _name: &str,
    _kind: ContentType,
    _options: &ContentOptions,
) -> Result<String, brush_core::Error> {
    Ok("echo: write arguments to standard output\n".into())
}

async fn shell() -> Result<Shell> {
    Ok(Shell::builder()
        .builtin(
            "echo",
            Registration {
                execute_func: echo,
                content_func: echo_help,
                disabled: false,
                special_builtin: false,
                declaration_builtin: false,
            },
        )
        .build()
        .await?)
}

async fn run(
    shell: &mut Shell,
    script: &str,
    options: PipelineOptions,
) -> Result<(u8, String, String)> {
    let dir = tempfile::tempdir()?;
    let out = dir.path().join("out");
    let err = dir.path().join("err");
    let mut params = shell.default_exec_params();
    params.pipeline_options = options;
    // A wrapping `{ script; } >out` would turn all of script into one outermost pipeline.
    params.set_fd(1, std::fs::File::create(&out)?.into());
    params.set_fd(2, std::fs::File::create(&err)?.into());
    let result = shell
        .run_string(script, &SourceInfo::default(), &params)
        .await?;
    Ok((
        result.exit_code.into(),
        std::fs::read_to_string(out)?,
        std::fs::read_to_string(err)?,
    ))
}

fn limits() -> ResourceLimits {
    ResourceLimits {
        memory: Some(MemoryLimit::new(512 * MIB)),
        cpu: Some(CpuLimit::new(0.5)),
        pids: Some(64),
    }
}

async fn available(shell: &mut Shell) -> Result<bool> {
    let (code, stdout, stderr) = run(
        shell,
        "/bin/true",
        PipelineOptions::default().with_resource_limits(limits()),
    )
    .await?;
    if stderr.contains("cgroup resource control unavailable") {
        assert_ne!(code, 0, "unavailable controls must fail closed");
        assert!(stdout.is_empty());
        eprintln!("skipping resource_limits_tests: {stderr}");
        return Ok(false);
    }
    assert_eq!(code, 0, "{stderr}");
    Ok(true)
}

async fn final_sample(
    events: &mut tokio::sync::mpsc::Receiver<ResourceEvent>,
) -> Result<ResourceSample> {
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = events.recv().await {
            if let ResourceEvent::Finished { sample, .. } = event {
                return Ok(sample);
            }
        }
        anyhow::bail!("monitor closed without Finished")
    })
    .await?
}

#[tokio::test]
async fn limits_apply_to_every_process_of_the_pipeline() -> Result<()> {
    let mut shell = shell().await?;
    if !available(&mut shell).await? {
        return Ok(());
    }
    let (code, stdout, stderr) = run(&mut shell,
        "f() { cat /proc/self/cgroup; }; sh -c 'd=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup); cat $d/memory.max $d/cpu.max $d/pids.max; cat /proc/self/cgroup' | { cat; f; }",
        PipelineOptions::default().with_resource_limits(limits())).await?;
    assert_eq!(code, 0, "{stderr}");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 5, "{stdout}");
    assert_eq!(&lines[..3], &["536870912", "50000 100000", "64"]);
    assert_eq!(lines[3], lines[4]);
    assert!(lines[3].contains(&format!("/brush-{}-", std::process::id())));
    let path = lines[3].strip_prefix("0::/").context("unified hierarchy")?;
    assert!(
        !Path::new("/sys/fs/cgroup").join(path).exists(),
        "pipeline cgroup was not removed"
    );
    eprintln!("limits: {} / {} / {}", lines[0], lines[1], lines[2]);
    Ok(())
}

#[tokio::test]
async fn each_outermost_pipeline_gets_its_own_cgroup() -> Result<()> {
    let mut shell = shell().await?;
    if !available(&mut shell).await? {
        return Ok(());
    }
    let (code, stdout, stderr) = run(
        &mut shell,
        "cat /proc/self/cgroup; cat /proc/self/cgroup",
        PipelineOptions::default().with_resource_limits(limits()),
    )
    .await?;
    assert_eq!(code, 0, "{stderr}");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{stdout}");
    assert_ne!(lines[0], lines[1]);
    assert!(lines.iter().all(|line| line.contains("/brush-")));
    Ok(())
}

#[tokio::test]
async fn memory_limit_kills_the_pipeline() -> Result<()> {
    let mut shell = shell().await?;
    if !available(&mut shell).await? {
        return Ok(());
    }
    let monitor = ResourceMonitorConfig::new(Duration::from_millis(10));
    let mut events = monitor.events().context("receiver taken once")?;
    let options = PipelineOptions::default()
        .with_resource_limits(ResourceLimits {
            memory: Some(MemoryLimit::new(32 * MIB)),
            ..ResourceLimits::default()
        })
        .with_resource_monitor(monitor);
    let (result, sample) = tokio::join!(
        run(
            &mut shell,
            "python3 -c 'bytearray(128 * 1024 * 1024)'",
            options
        ),
        final_sample(&mut events),
    );
    let (code, _, stderr) = result?;
    let sample = sample?;
    assert_eq!(code, 137, "{stderr}");
    assert!(sample.oom_kills >= 1, "{sample:?}");
    assert!(sample.memory_peak_bytes <= 36 * MIB, "{sample:?}");
    eprintln!(
        "OOM: exit={code}, oom_kills={}, memory_peak_bytes={}",
        sample.oom_kills, sample.memory_peak_bytes
    );
    Ok(())
}

#[tokio::test]
async fn monitor_streams_samples_then_a_final_report() -> Result<()> {
    let mut shell = shell().await?;
    if !available(&mut shell).await? {
        return Ok(());
    }
    let monitor = ResourceMonitorConfig::new(Duration::from_millis(10));
    let mut events = monitor.events().context("receiver taken once")?;
    assert!(
        monitor.clone().events().is_none(),
        "clones share receiver ownership"
    );
    let options = PipelineOptions::default()
        .with_resource_limits(ResourceLimits {
            cpu: Some(CpuLimit::new(0.2)),
            ..ResourceLimits::default()
        })
        .with_resource_monitor(monitor);
    let collect = async {
        tokio::time::timeout(Duration::from_secs(30), async {
            let Some(ResourceEvent::Started {
                pipeline_id,
                command,
            }) = events.recv().await
            else {
                anyhow::bail!("first event must be Started");
            };
            assert!(command.contains("sh -c"), "{command}");
            let mut sampled = false;
            loop {
                match events.recv().await {
                    Some(ResourceEvent::Sample {
                        pipeline_id: id, ..
                    }) => {
                        assert_eq!(id, pipeline_id);
                        sampled = true;
                    }
                    Some(ResourceEvent::Finished {
                        pipeline_id: id,
                        sample,
                    }) => {
                        assert_eq!(id, pipeline_id);
                        assert!(sampled, "no periodic samples");
                        return Ok(sample);
                    }
                    event => anyhow::bail!("unexpected monitor event: {event:?}"),
                }
            }
        })
        .await?
    };
    let (result, sample): (_, Result<ResourceSample>) = tokio::join!(
        run(
            &mut shell,
            "sh -c 'i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done'",
            options
        ),
        collect,
    );
    let (code, _, stderr) = result?;
    assert_eq!(code, 0, "{stderr}");
    let sample = sample?;
    assert!(sample.cpu_usage > Duration::ZERO, "{sample:?}");
    assert!(sample.cpu_throttled > Duration::ZERO, "{sample:?}");
    assert!(sample.elapsed > Duration::ZERO);
    assert_eq!(sample.pids, 0);
    Ok(())
}

#[tokio::test]
async fn builtin_only_pipelines_create_no_cgroup_and_no_events() -> Result<()> {
    // This invariant must hold even without delegation, so do not skip this test.
    let mut shell = shell().await?;
    let monitor = ResourceMonitorConfig::new(Duration::from_millis(10));
    let mut events = monitor.events().context("receiver taken once")?;
    let (code, stdout, stderr) = run(
        &mut shell,
        "x=1; echo \"$x\"",
        PipelineOptions::default()
            .with_resource_limits(limits())
            .with_resource_monitor(monitor.clone()),
    )
    .await?;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "1\n");
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[tokio::test]
async fn late_process_substitution_stays_enforced() -> Result<()> {
    let mut shell = shell().await?;
    if !available(&mut shell).await? {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let late = dir.path().join("late");
    let script = format!(
        "true <(sleep 0.2; cat /proc/self/cgroup > {}); sleep 0.8",
        late.display()
    );
    let (code, _, stderr) = run(
        &mut shell,
        &script,
        PipelineOptions::default().with_resource_limits(limits()),
    )
    .await?;
    assert_eq!(code, 0, "{stderr}");
    assert!(std::fs::read_to_string(late)?.contains("/brush-"));
    // Trigger the documented deferred-directory sweep after the late process has exited.
    let (code, _, stderr) = run(
        &mut shell,
        "/bin/true",
        PipelineOptions::default().with_resource_limits(limits()),
    )
    .await?;
    assert_eq!(code, 0, "{stderr}");
    Ok(())
}
