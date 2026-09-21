use rmux_core::events::SubscriptionLimits;

use crate::signals::SignalWatcher;
#[cfg(unix)]
use crate::unix_socket::SocketFileIdentity;
#[cfg(unix)]
use crate::unix_socket_access::UnixSocketAccessController;
use crate::ConfigLoadOptions;

pub(crate) struct ServeOptions {
    pub(crate) server_signals: Option<SignalWatcher>,
    pub(crate) config_load: ConfigLoadOptions,
    pub(crate) subscription_limits: SubscriptionLimits,
    pub(crate) owner_uid: u32,
    pub(crate) web_frontend: Option<String>,
    pub(crate) web_port: u16,
    pub(crate) web_port_explicit: bool,
    pub(crate) web_required: bool,
    #[cfg(unix)]
    pub(crate) socket_identity: Option<SocketFileIdentity>,
    #[cfg(unix)]
    pub(crate) socket_access: Option<UnixSocketAccessController>,
    /// The daemon's in-process I/O facade, shared by every handler.
    ///
    /// Not optional in production: every pane, popup and workload helper runs through it, and a
    /// daemon without one would have no way to start a shell at all.
    pub(crate) io: Option<crate::io::ShellIo>,
    /// The shell engine's observation queue, drained by one owned consumer task.
    pub(crate) events:
        Option<tokio::sync::mpsc::UnboundedReceiver<crate::shell_frontend::FrontendMessage>>,
}

impl ServeOptions {
    pub(crate) fn new(
        config_load: ConfigLoadOptions,
        subscription_limits: SubscriptionLimits,
        owner_uid: u32,
    ) -> Self {
        Self {
            server_signals: None,
            config_load,
            subscription_limits,
            owner_uid,
            web_frontend: None,
            web_port: 9777,
            web_port_explicit: false,
            web_required: false,
            #[cfg(unix)]
            socket_identity: None,
            #[cfg(unix)]
            socket_access: None,
            io: None,
            events: None,
        }
    }

    /// Attaches the I/O facade and the observation queue this daemon serves.
    pub(crate) fn with_shell_io(
        mut self,
        io: crate::io::ShellIo,
        events: tokio::sync::mpsc::UnboundedReceiver<crate::shell_frontend::FrontendMessage>,
    ) -> Self {
        self.io = Some(io);
        self.events = Some(events);
        self
    }

    pub(crate) fn with_web_options(
        mut self,
        port: u16,
        frontend: Option<String>,
        required: bool,
        port_explicit: bool,
    ) -> Self {
        self.web_port = port;
        self.web_frontend = frontend;
        self.web_required = required;
        self.web_port_explicit = port_explicit;
        self
    }

    #[cfg(unix)]
    pub(crate) fn with_socket_identity(
        mut self,
        socket_identity: Option<SocketFileIdentity>,
    ) -> Self {
        self.socket_identity = socket_identity;
        self
    }

    #[cfg(unix)]
    pub(crate) fn with_socket_access(mut self, socket_access: UnixSocketAccessController) -> Self {
        self.socket_access = Some(socket_access);
        self
    }

    #[cfg(unix)]
    pub(crate) fn with_server_signals(mut self, server_signals: SignalWatcher) -> Self {
        self.server_signals = Some(server_signals);
        self
    }
}
