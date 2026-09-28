//! Multiplexing owns names, streams and receipts. Shell owns every transaction and source lease.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, Weak};

use crate::shellmux::error::MuxError;
use crate::shellmux::frontend::{FrontendEvent, ShellFrontend, lock_frontend, notify};
use crate::shellmux::ids::{JobDir, Principal, ShellId};
use crate::shellmux::jobs::{ShellRegistry, validate_size};
use crate::shellmux::types::MuxProfile;
use crate::{OpenFile, ShellBuilder, ShellEnvironment, ShellFd};
use tokio::sync::Notify;

/// Presentation identity and logical source location of one shell instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sandbox {
    /// Reusable display/lookup name; never a capability principal.
    pub id: ShellId,
    /// Canonical source root: the Git work-tree root containing the initial directory, or that
    /// directory itself outside a work tree. Fixed for the shell's lifetime.
    pub seed: PathBuf,
    /// Initial directory relative to `seed`; unchanged by later `cd`.
    pub dir: JobDir,
    /// The shell's stable Junco principal, also used for retained-handle identity.
    pub uid: Principal,
}

/// A collection of ordinary managed shells and their terminal/pipe lifecycles.
pub struct ShellMux {
    profile: MuxProfile,
    builder: Arc<dyn Fn() -> ShellBuilder + Send + Sync>,
    pub(crate) shells: Mutex<ShellRegistry>,
    pub(crate) launched: Notify,
    pub(crate) tasks: Mutex<tokio::task::JoinSet<()>>,
    pub(crate) runtime: tokio::runtime::Handle,
    pub(crate) resize_lock: tokio::sync::Mutex<()>,
    pub(crate) command_counter: AtomicU64,
    frontend: Arc<Mutex<dyn ShellFrontend>>,
}
impl std::fmt::Debug for ShellMux {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellMux")
            .field("jobs", &self.jobs().len())
            .finish_non_exhaustive()
    }
}
impl ShellMux {
    /// Creates a mux with normal Shell construction and the frontend's initial geometry.
    pub fn new<V: ShellFrontend>(
        profile: MuxProfile,
        frontend: Arc<Mutex<V>>,
    ) -> Result<Arc<Self>, MuxError> {
        Self::with_builder_factory(profile, frontend, Arc::new(crate::Shell::builder))
    }
    pub(crate) fn with_builder_factory<V: ShellFrontend>(
        profile: MuxProfile,
        frontend: Arc<Mutex<V>>,
        builder: Arc<dyn Fn() -> ShellBuilder + Send + Sync>,
    ) -> Result<Arc<Self>, MuxError> {
        let frontend: Arc<Mutex<dyn ShellFrontend>> = frontend;
        let (rows, cols) = lock_frontend(&frontend).size();
        validate_size(rows, cols)?;
        let runtime = tokio::runtime::Handle::current();
        let mux = Arc::new(Self {
            profile,
            builder,
            shells: Mutex::new(ShellRegistry::new(rows, cols)),
            launched: Notify::new(),
            tasks: Mutex::new(tokio::task::JoinSet::new()),
            runtime,
            resize_lock: tokio::sync::Mutex::new(()),
            command_counter: AtomicU64::new(1),
            frontend: Arc::clone(&frontend),
        });
        let mut bound = lock_frontend(&frontend);
        bound.bind(Arc::downgrade(&mux));
        let _ = bound.update(FrontendEvent::Changed);
        drop(bound);
        Ok(mux)
    }
    pub(crate) fn announce(&self, event: FrontendEvent<'_>) {
        let _ = notify(&self.frontend, event);
    }
    pub(crate) fn frontend(&self) -> Arc<Mutex<dyn ShellFrontend>> {
        Arc::clone(&self.frontend)
    }
    pub(crate) fn detach(&self) {
        lock_frontend(&self.frontend).bind(Weak::new());
    }
    /// Whether shells built by this mux include the named extra or managed builtin.
    pub fn has_builtin(&self, name: &str) -> bool {
        self.profile.builtins.contains_key(name) || matches!(name, "git" | "exec")
    }

    pub(crate) async fn build_shell(
        &self,
        id: ShellId,
        directory: &Path,
        fds: HashMap<ShellFd, OpenFile>,
        environment: Option<ShellEnvironment>,
    ) -> Result<Arc<crate::Shell>, MuxError> {
        let mut builder = (self.builder)()
            .working_dir(directory.to_path_buf())
            .fds(fds)
            .interactive(false)
            .no_editing(true)
            .external_cmd_leads_session(true)
            .enable_option("monitor".into())
            .sandbox_policy(self.profile.sandbox_policy.clone());
        if let Some(environment) = environment {
            builder = builder.environment(environment);
        } else {
            for (name, variable) in self.profile.environment.iter() {
                builder = builder.var(name.clone(), variable.clone());
            }
        }
        for (name, registration) in &self.profile.builtins {
            builder = builder.builtin(name.clone(), registration.clone());
        }
        // Startup already routes under the name the mux reserved.
        builder.sandbox_id = Some(id);
        Ok(Arc::new(Box::pin(builder.build()).await?))
    }
}
