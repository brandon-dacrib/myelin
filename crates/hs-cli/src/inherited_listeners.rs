//! Adopting the listening sockets a parent process bound and passed down to `hs serve`, named in
//! [`crate::serve::INHERITED_LISTENERS_ENV`] (see its doc comment for why).
//!
//! Every descriptor is checked before any is taken: it must be above standard error, listed
//! once, open, a socket, a stream socket, listening, and bound to an IPv4 or IPv6 address. Only
//! then does this process take ownership of it (closing it when the listener is dropped) and set
//! close-on-exec on it again, so that nothing `hs serve` itself starts inherits it in turn.

use crate::serve::InheritedListenersError;

/// The listeners named by `value`, a comma-separated list of descriptor numbers.
#[cfg(unix)]
pub(crate) fn adopt(value: &str) -> Result<Vec<std::net::TcpListener>, InheritedListenersError> {
    use std::os::fd::FromRawFd as _;

    let mut fds: Vec<i32> = Vec::new();
    for entry in value.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let fd = entry
            .parse::<i32>()
            .ok()
            .filter(|fd| *fd > 2 && !fds.contains(fd))
            .ok_or_else(|| InheritedListenersError::NotANumber(entry.to_owned()))?;
        fds.push(fd);
    }
    for &fd in &fds {
        check(fd).map_err(|source| InheritedListenersError::NotAListener { fd, source })?;
    }
    let mut listeners = Vec::with_capacity(fds.len());
    for fd in fds {
        // SAFETY: `check` has just found `fd` open and a listening TCP socket. Nothing else in
        // this process owns it: it was inherited across `exec` (the parent cleared close-on-exec
        // for exactly this purpose), every descriptor in the list is distinct, and this runs once
        // per process, at the start of `hs serve`, before anything else could have wrapped it.
        let listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
        set_cloexec(fd).map_err(|source| InheritedListenersError::NotAListener { fd, source })?;
        listeners.push(listener);
    }
    Ok(listeners)
}

/// Not Unix: there is nothing to adopt descriptors with.
#[cfg(not(unix))]
pub(crate) fn adopt(_value: &str) -> Result<Vec<std::net::TcpListener>, InheritedListenersError> {
    Err(InheritedListenersError::Unsupported)
}

/// Whether `fd` is an open, listening TCP socket, without taking ownership of it.
#[cfg(unix)]
fn check(fd: i32) -> std::io::Result<()> {
    let invalid = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, what);
    // SAFETY: `fcntl(F_GETFD)` only reads the descriptor's flags; on a closed descriptor it
    // fails with `EBADF` and touches nothing.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if socket_option(fd, libc::SO_TYPE)? != libc::SOCK_STREAM {
        return Err(invalid("not a stream socket"));
    }
    match socket_option(fd, libc::SO_ACCEPTCONN) {
        Ok(0) => return Err(invalid("not listening")),
        Ok(_) => {}
        // macOS defines the option but `getsockopt` does not answer it. There, a socket with a
        // peer is a connection, not a listener; one without is taken as listening (if it is
        // merely bound, accepting on it fails and the server's log says so).
        Err(e) if e.raw_os_error() == Some(libc::ENOPROTOOPT) => {
            if has_peer(fd)? {
                return Err(invalid("a connection, not a listener"));
            }
        }
        Err(e) => return Err(e),
    }
    // SAFETY: `storage` is a zeroed `sockaddr_storage`, large enough for any address, and `len`
    // says so; `getsockname` writes at most `len` bytes into it.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = socklen_of::<libc::sockaddr_storage>();
    let rc = unsafe {
        libc::getsockname(
            fd,
            std::ptr::from_mut(&mut storage).cast::<libc::sockaddr>(),
            &raw mut len,
        )
    };
    if rc == -1 {
        return Err(std::io::Error::last_os_error());
    }
    match i32::from(storage.ss_family) {
        libc::AF_INET | libc::AF_INET6 => Ok(()),
        _ => Err(invalid("not an IPv4 or IPv6 socket")),
    }
}

/// Whether the socket `fd` is connected to a peer.
#[cfg(unix)]
fn has_peer(fd: i32) -> std::io::Result<bool> {
    // SAFETY: as in `check`'s `getsockname`: a zeroed `sockaddr_storage` and its size.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = socklen_of::<libc::sockaddr_storage>();
    let rc = unsafe {
        libc::getpeername(
            fd,
            std::ptr::from_mut(&mut storage).cast::<libc::sockaddr>(),
            &raw mut len,
        )
    };
    if rc == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOTCONN) {
        Ok(false)
    } else {
        Err(error)
    }
}

/// One integer socket option of `fd`.
#[cfg(unix)]
fn socket_option(fd: i32, option: libc::c_int) -> std::io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = socklen_of::<libc::c_int>();
    // SAFETY: `value` is a `c_int` and `len` is its size, which is what these options write;
    // a descriptor that is not a socket fails with `ENOTSOCK` and nothing is written.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            std::ptr::from_mut(&mut value).cast::<libc::c_void>(),
            &raw mut len,
        )
    };
    if rc == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(value)
}

/// Sets close-on-exec on `fd` again.
#[cfg(unix)]
fn set_cloexec(fd: i32) -> std::io::Result<()> {
    // SAFETY: reading and setting a descriptor's flags; `fd` is open (checked) and owned by the
    // listener just made from it.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `size_of::<T>()` as a `socklen_t` (every `T` used here is a few bytes to a few hundred).
#[cfg(unix)]
fn socklen_of<T>() -> libc::socklen_t {
    libc::socklen_t::try_from(std::mem::size_of::<T>()).unwrap_or(libc::socklen_t::MAX)
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::fd::{AsRawFd as _, IntoRawFd as _};

    use super::*;

    #[test]
    fn a_listening_socket_is_adopted_and_close_on_exec_again() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let fd = listener.into_raw_fd();
        // As a child that inherited it would see it.
        unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        let adopted = adopt(&format!(" {fd} ,")).unwrap();
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0].local_addr().unwrap(), addr);
        assert_eq!(adopted[0].as_raw_fd(), fd);
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "close-on-exec is set again");
    }

    #[test]
    fn what_is_not_a_listening_tcp_socket_is_refused_and_left_open() {
        // Standard streams, garbage and repeats are refused before anything is looked at.
        for value in ["0", "2", "-4", "x", "3,3"] {
            assert!(
                matches!(adopt(value), Err(InheritedListenersError::NotANumber(_))),
                "{value}"
            );
        }
        // A file, a UDP socket and a connected stream are not listeners; each is
        // still open afterwards (nothing took ownership of it).
        let file = tempfile::tempfile().unwrap();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
        for fd in [file.as_raw_fd(), udp.as_raw_fd(), stream.as_raw_fd()] {
            assert!(
                matches!(
                    adopt(&fd.to_string()),
                    Err(InheritedListenersError::NotAListener { .. })
                ),
                "{fd}"
            );
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_GETFD) },
                -1,
                "{fd} is open"
            );
        }
    }
}
