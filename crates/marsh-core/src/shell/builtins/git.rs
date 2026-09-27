//! The managed git command; argument parsing and actual Git effects remain in gitexec/gitcmd.

use brush_core::builtins::Command;
use brush_core::{ExecutionContext, ExecutionResult, ShellExtensions};

#[derive(clap::Parser)]
pub(super) struct GitBuiltin {
    #[clap(allow_hyphen_values = true, num_args = 0..)]
    args: Vec<String>,
}
impl Command for GitBuiltin {
    type Error = brush_core::Error;
    fn new<I: IntoIterator<Item = String>>(args: I) -> Result<Self, clap::Error> {
        Ok(Self {
            args: args.into_iter().collect(),
        })
    }
    async fn execute<SE: ShellExtensions>(
        &self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let snapshot = super::current_context()
            .and_then(|context| context.snapshot())
            .ok_or_else(|| {
                brush_core::Error::from(brush_core::ErrorKind::InternalError(
                    "git requires an active managed command".into(),
                ))
            })?;
        super::gitexec::run(context, self.args.clone(), snapshot).await
    }
}
