//! Public execution verdicts without transaction handles.

use super::policy::Denial;
use brush_core::ExecutionResult;
use std::path::PathBuf;

/// Why an ordinary shell operation could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellErrorKind {
    /// Another operation already owns this shell.
    Busy,
    /// The shell no longer accepts operations.
    Closed,
    /// The complete capability batch was refused.
    Denied {
        /// All rejected requests and their explanations.
        denials: Vec<Denial>,
    },
    /// A concurrent publication invalidated the command's observed view. Never auto-retried.
    Stale {
        /// Logical source paths whose newer versions conflict.
        paths: Vec<PathBuf>,
    },
    /// Cancellation discarded the command's private effects.
    Interrupted,
    /// The effect cannot be represented safely by the supported transaction contract.
    Unsupported,
    /// An interpreter, native observation or storage operation failed.
    Infrastructure,
}

/// One failed operation, preserving any native process status obtained before finalization failed.
pub struct ShellError {
    kind: ShellErrorKind,
    result: Option<ExecutionResult>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl ShellError {
    /// The consumer-visible verdict.
    pub const fn kind(&self) -> &ShellErrorKind {
        &self.kind
    }
    /// Native status and control flow, when execution completed before the failure.
    pub const fn execution_result(&self) -> Option<&ExecutionResult> {
        self.result.as_ref()
    }

    pub(crate) const fn new(kind: ShellErrorKind) -> Self {
        Self {
            kind,
            result: None,
            source: None,
        }
    }
    pub(super) fn caused(
        kind: ShellErrorKind,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            result: None,
            source: Some(Box::new(cause)),
        }
    }
    pub(super) fn infrastructure(message: impl Into<String>) -> Self {
        Self::caused(
            ShellErrorKind::Infrastructure,
            std::io::Error::other(message.into()),
        )
    }
    pub(super) fn unsupported(message: impl Into<String>) -> Self {
        Self::caused(
            ShellErrorKind::Unsupported,
            std::io::Error::other(message.into()),
        )
    }
    pub(super) const fn with_result(mut self, result: Option<ExecutionResult>) -> Self {
        self.result = result;
        self
    }
}

impl std::fmt::Debug for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellError")
            .field("kind", &self.kind)
            .field(
                "exit_code",
                &self
                    .result
                    .as_ref()
                    .map(|result| u8::from(result.exit_code)),
            )
            .field("source", &self.source)
            .finish()
    }
}
impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            ShellErrorKind::Busy => f.write_str("shell is busy")?,
            ShellErrorKind::Closed => f.write_str("shell is closed")?,
            ShellErrorKind::Denied { denials } => {
                f.write_str("capability denied")?;
                for denial in denials {
                    write!(f, "\n{denial}")?;
                }
            }
            ShellErrorKind::Stale { paths } => {
                f.write_str("command view is stale; effects were not published")?;
                for path in paths {
                    write!(f, "\n{}", path.display())?;
                }
            }
            ShellErrorKind::Interrupted => {
                f.write_str("command interrupted; effects were discarded")?;
            }
            ShellErrorKind::Unsupported => f.write_str("unsupported command effect")?,
            ShellErrorKind::Infrastructure => f.write_str("shell infrastructure failure")?,
        }
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}
impl std::error::Error for ShellError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| &**source as &(dyn std::error::Error + 'static))
    }
}
macro_rules! infrastructure {
    ($($error:ty),+ $(,)?) => {$ (
        impl From<$error> for ShellError {
            fn from(error: $error) -> Self { Self::caused(ShellErrorKind::Infrastructure, error) }
        }
    )+};
}
infrastructure!(
    std::io::Error,
    marsh_btrfs::Error,
    marsh_wal::Error,
    brush_core::Error,
    serde_json::Error
);
