//! Ports for servers a test starts, reserved without ever being released.
//!
//! A test that writes a server's port into its configuration (a federation test's
//! `server_name: 127.0.0.1:<port>`, a cluster replica's mesh address) needs the port before the
//! server starts. The old way -- bind `127.0.0.1:0`, read the port, drop the listener, have the
//! server bind the port again -- leaves a window in which anything else on the machine can take
//! the port: another test's `bind(0)`, or the ephemeral source port of any outgoing connection.
//! Under a loaded run (the merge gate, a CI runner) that window was long enough to lose, and a
//! server failed to boot with `Bind { addr: "127.0.0.1:39139", .. AddrInUse }`.
//!
//! Here the listener bound to port 0 is never dropped. It is handed to the server:
//!
//! - **in-process** (`hs_cli::serve::spawn_serve`): through `ServeOptions::listeners`, as
//!   `hs_cli::serve::PreboundListeners::new([reserved.into_listener()])`;
//! - **the `hs` binary as a child process**: with [`pass_to`], which lets the child inherit the
//!   listening socket and names it in [`INHERITED_LISTENERS_ENV`]; `hs serve` serves it in place
//!   of binding the configured listener (or cluster mesh) whose address it is bound to;
//! - **a cluster mesh server** (`hs_cluster::mesh::MeshServer`): with `with_listener`.
//!
//! For a server that can be handed no listener (a third-party one, a container),
//! [`retry_on_addr_in_use`] boots it on a fresh port when its port was taken.
//!
//! A port a test wants nothing to listen on (to see a connection refused) is still best had by
//! binding and dropping: that is not a race the test can lose in a way that matters.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};

/// The environment variable `hs serve` reads inherited listening sockets from: a
/// comma-separated list of descriptor numbers. The same name as hs-cli's
/// `hs_cli::serve::INHERITED_LISTENERS_ENV` (hs-cli's tests check the two agree; this crate
/// cannot depend on hs-cli).
pub const INHERITED_LISTENERS_ENV: &str = "HS_INHERITED_LISTENER_FDS";

/// A TCP listener bound to a port the system picked, held until it is handed to the server that
/// will serve it.
#[derive(Debug)]
pub struct ReservedPort {
    listener: TcpListener,
    addr: SocketAddr,
}

impl ReservedPort {
    /// Binds `ip` on a port the system picks.
    ///
    /// # Errors
    /// Binding failed.
    pub fn bind(ip: impl Into<std::net::IpAddr>) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::new(ip.into(), 0))?;
        let addr = listener.local_addr()?;
        Ok(Self { listener, addr })
    }

    /// The port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The address the listener is bound to (`127.0.0.1:<port>` from [`reserve`]).
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The listener, to hand to the server.
    #[must_use]
    pub fn into_listener(self) -> TcpListener {
        self.listener
    }

    /// A second handle on the same listening socket, for a test that restarts a server on the
    /// same port: it keeps this one and hands a clone to each start, so the port is never
    /// released. While no server runs, connections to the port wait in its backlog rather than
    /// being refused; a test that needs them refused while the server is down should drop every
    /// handle and have the restart bind the port itself.
    ///
    /// # Errors
    /// Duplicating the descriptor failed.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            listener: self.listener.try_clone()?,
            addr: self.addr,
        })
    }
}

impl From<ReservedPort> for TcpListener {
    fn from(reserved: ReservedPort) -> Self {
        reserved.into_listener()
    }
}

/// A port on `127.0.0.1`, the address every listener in this workspace's test configurations
/// binds.
///
/// # Panics
/// If the system will not bind a loopback port at all, which leaves a test nothing to run on.
#[must_use]
pub fn reserve() -> ReservedPort {
    ReservedPort::bind(Ipv4Addr::LOCALHOST).expect("binding a loopback port the system picks")
}

/// A port on `0.0.0.0`, for what binds every interface: `hs serve`'s cluster mesh listens on
/// `0.0.0.0:<cluster.mesh.port>` whatever the listeners bind, so a mesh port handed to it must be
/// reserved here to be matched.
///
/// # Panics
/// As [`reserve`].
#[must_use]
pub fn reserve_wildcard() -> ReservedPort {
    ReservedPort::bind(Ipv4Addr::UNSPECIFIED).expect("binding a port the system picks")
}

/// Has the `hs serve` that `command` starts inherit `listeners`, named in
/// [`INHERITED_LISTENERS_ENV`].
///
/// The sockets stay close-on-exec in this process (so no other child spawned meanwhile, by this
/// test or another running beside it, inherits them) and are made inheritable only in the
/// forked child, just before it runs `hs`. The `command` keeps this process's copies open until
/// it is dropped, so drop it once the child is spawned: while a copy is open here, the port
/// accepts connections (into its backlog) even after the child has gone.
///
/// One call per command: a second call replaces the variable.
#[cfg(unix)]
pub fn pass_to(
    command: &mut std::process::Command,
    listeners: impl IntoIterator<Item = ReservedPort>,
) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt as _;

    let listeners: Vec<TcpListener> = listeners
        .into_iter()
        .map(ReservedPort::into_listener)
        .collect();
    let fds: Vec<std::os::fd::RawFd> = listeners.iter().map(AsRawFd::as_raw_fd).collect();
    let names: Vec<String> = fds.iter().map(ToString::to_string).collect();
    command.env(INHERITED_LISTENERS_ENV, names.join(","));
    // SAFETY: the closure runs in the forked child before `exec`, where only async-signal-safe
    // functions may be called. It calls `fcntl` alone, allocates nothing (it reads `fds`, built
    // above in the parent) and returns an `io::Error` built from `errno`, which does not
    // allocate either. Clearing `FD_CLOEXEC` touches only the child's descriptor table. The
    // listeners it captures are what keep the descriptors open until the fork.
    unsafe {
        command.pre_exec(move || {
            let _keep_open = &listeners;
            for &fd in &fds {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Not Unix: the listeners are released and the child binds the configured ports itself, as
/// before this module (the race is back, but the test still runs).
#[cfg(not(unix))]
pub fn pass_to(
    _command: &mut std::process::Command,
    listeners: impl IntoIterator<Item = ReservedPort>,
) {
    drop(listeners.into_iter().collect::<Vec<_>>());
}

/// Whether `error`, or anything in its chain of sources, is a failure to bind an address that
/// is in use.
#[must_use]
pub fn is_addr_in_use(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(io) = error.downcast_ref::<io::Error>()
            && io.kind() == io::ErrorKind::AddrInUse
        {
            return true;
        }
        let text = error.to_string();
        if text.contains("Address already in use") || text.contains("AddrInUse") {
            return true;
        }
        current = error.source();
    }
    false
}

/// Starts a server that can only be told a port number, retrying on a fresh port while
/// `start` fails because its port was taken ([`is_addr_in_use`]), up to `attempts` times in all.
///
/// For servers that cannot be handed a listener: a third-party one, a container. Anything else
/// should be handed its reserved listener instead (this module's docs).
///
/// # Errors
/// The last attempt's error, or the first error that is not about the port being in use.
pub async fn retry_on_addr_in_use<T, E, F, Fut>(attempts: usize, mut start: F) -> Result<T, E>
where
    F: FnMut(u16) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::error::Error + 'static,
{
    let mut attempt = 1;
    loop {
        let port = {
            // Bound and dropped: the race this function exists to survive.
            let probe = ReservedPort::bind(Ipv4Addr::LOCALHOST);
            probe.map_or(0, |probe| probe.port())
        };
        match start(port).await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < attempts && is_addr_in_use(&error) => {
                tracing::warn!(port, attempt, %error, "port taken before the server bound it; retrying on another");
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reserved_port_is_held_until_it_is_handed_over() {
        let reserved = reserve();
        assert_eq!(reserved.addr().ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(reserved.port(), 0);
        // Nobody else can have it meanwhile.
        let err = TcpListener::bind(reserved.addr()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        // And the listener handed over is the one bound to it.
        let addr = reserved.addr();
        let listener = reserved.into_listener();
        assert_eq!(listener.local_addr().unwrap(), addr);
        let wildcard = reserve_wildcard();
        assert!(wildcard.addr().ip().is_unspecified());
    }

    #[test]
    fn addr_in_use_is_found_anywhere_in_the_chain() {
        #[derive(Debug)]
        struct Wrapped(io::Error);
        impl std::fmt::Display for Wrapped {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("failed to bind listener")
            }
        }
        impl std::error::Error for Wrapped {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let in_use = Wrapped(io::Error::from(io::ErrorKind::AddrInUse));
        assert!(is_addr_in_use(&in_use));
        let refused = Wrapped(io::Error::from(io::ErrorKind::ConnectionRefused));
        assert!(!is_addr_in_use(&refused));
        let text = io::Error::other("bind: Address already in use (os error 48)");
        assert!(is_addr_in_use(&text));
    }

    #[tokio::test]
    async fn a_start_that_lost_its_port_is_retried_on_another() {
        let mut ports = Vec::new();
        let started = retry_on_addr_in_use(3, |port| {
            ports.push(port);
            let attempt = ports.len();
            async move {
                if attempt < 3 {
                    Err(io::Error::from(io::ErrorKind::AddrInUse))
                } else {
                    Ok(port)
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(ports.len(), 3);
        assert_eq!(started, ports[2]);

        // Out of attempts: the last error. Not about the port: no retry.
        let mut calls = 0;
        let err = retry_on_addr_in_use(2, |_| {
            calls += 1;
            async { Err::<(), _>(io::Error::from(io::ErrorKind::AddrInUse)) }
        })
        .await
        .unwrap_err();
        assert_eq!((calls, err.kind()), (2, io::ErrorKind::AddrInUse));
        let mut calls = 0;
        let err = retry_on_addr_in_use(5, |_| {
            calls += 1;
            async { Err::<(), _>(io::Error::from(io::ErrorKind::PermissionDenied)) }
        })
        .await
        .unwrap_err();
        assert_eq!((calls, err.kind()), (1, io::ErrorKind::PermissionDenied));
    }

    /// The child sees the listener at the number named in the variable, and a child spawned
    /// without [`pass_to`] does not inherit it.
    #[cfg(unix)]
    #[test]
    fn a_passed_listener_is_inherited_by_that_child_only() {
        let reserved = reserve();
        let fd = {
            use std::os::fd::AsRawFd as _;
            reserved.listener.as_raw_fd()
        };
        let probe = format!(
            "test \"${INHERITED_LISTENERS_ENV}\" = {fd} && test -e /dev/fd/{fd} && echo inherited"
        );
        // A sibling spawned while the listener is held, without it.
        let sibling = std::process::Command::new("sh")
            .args([
                "-c",
                &format!("test -e /dev/fd/{fd} && echo leaked || echo clean"),
            ])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&sibling.stdout).trim(), "clean");

        let mut command = std::process::Command::new("sh");
        command.args(["-c", &probe]);
        pass_to(&mut command, [reserved]);
        let output = command.output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "inherited",
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
