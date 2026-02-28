use std::io;

/// Utility for determining how what's left on a socket queue:
///
/// Why this exists:
/// We sometimes need an OS-level “am I building up outbound pressure?” metric.
/// In an ideal world we’d get this from the async stack, but tokio + tungstenite add layers
/// (framing, buffering, backpressure propagation, task scheduling) that make it hard to prove
/// you’re observing the kernel’s send buffer state rather than an intermediate userspace queue,
/// and hard to do it safely without reaching for internal types or racy instrumentation.
/// This function punches through all of that: given a real socket FD, ask the kernel.
///
/// Caveat: Linux (`SIOCOUTQ`) and macOS (`SO_NWRITE`) aren’t guaranteed to have identical semantics;
/// treat the value as a backpressure severity proxy, not a cross-OS invariant.
pub fn os_sendq_bytes(fd: i32) -> io::Result<u32> {
    #[cfg(target_os = "linux")]
    {
        use nix::libc;
        use std::io;
        let mut out: libc::c_int = 0;
        let rc = unsafe { libc::ioctl(fd, libc::TIOCOUTQ as _, &mut out) };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(out as u32)
    }

    #[cfg(target_os = "macos")]
    {
        use nix::libc;
        use std::mem;
        let mut out: libc::c_int = 0;
        let mut len: libc::socklen_t = mem::size_of_val(&out) as libc::socklen_t;

        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_NWRITE,
                (&mut out as *mut libc::c_int).cast(),
                &mut len as *mut libc::socklen_t,
            )
        };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(out as u32)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = fd;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "os_sendq_bytes: unsupported OS",
        ))
    }
}
