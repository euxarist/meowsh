//! Thin wrappers over file descriptors. Builtins write straight to fd 1
//! and fd 2 so redirections apply and nothing sits in a buffer across
//! `fork`.

use std::io;

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Writes all of `bytes` to `fd`, retrying on EINTR.
pub fn write_fd(fd: i32, bytes: &[u8]) -> io::Result<()> {
    let mut off = 0;
    while off < bytes.len() {
        let n = unsafe {
            libc::write(
                fd,
                bytes[off..].as_ptr() as *const libc::c_void,
                bytes.len() - off,
            )
        };
        if n < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        off += n as usize;
    }
    Ok(())
}

/// Writes to standard output. Returns false on a write error, which
/// builtins report as a nonzero status.
pub fn out(s: &str) -> bool {
    write_fd(1, &crate::shell::zstr_to_bytes(s)).is_ok()
}

/// Writes a line to standard error.
pub fn err_line(s: &str) {
    let mut bytes = crate::shell::zstr_to_bytes(s);
    bytes.push(b'\n');
    let _ = write_fd(2, &bytes);
}

/// Duplicates `fd` to the lowest free descriptor >= 10, close-on-exec.
pub fn dup_high(fd: i32) -> i32 {
    unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) }
}

pub fn set_cloexec(fd: i32) {
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
}

/// A pipe whose ends are close-on-exec.
pub fn pipe_cloexec() -> io::Result<(i32, i32)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    set_cloexec(fds[0]);
    set_cloexec(fds[1]);
    Ok((fds[0], fds[1]))
}

pub fn close(fd: i32) {
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
    }
}

/// Reads everything from `fd` until end of file.
pub fn read_all(fd: i32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            break;
        }
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    out
}

/// Reads one byte from `fd`. Ok(None) at end of file.
pub fn read_byte(fd: i32) -> io::Result<Option<u8>> {
    let mut b = 0u8;
    loop {
        let n = unsafe { libc::read(fd, &mut b as *mut u8 as *mut libc::c_void, 1) };
        if n < 0 {
            if errno() == libc::EINTR {
                if crate::trap::has_pending() {
                    return Err(io::Error::from_raw_os_error(libc::EINTR));
                }
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        return Ok(if n == 0 { None } else { Some(b) });
    }
}

/// Reads input one line at a time without consuming bytes past the
/// newline, so commands run by the shell (such as `read`) see the rest of
/// the input. Seekable inputs are read in blocks and rewound.
pub struct LineReader {
    fd: i32,
    seekable: bool,
}

impl LineReader {
    pub fn new(fd: i32) -> Self {
        let seekable = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) } >= 0;
        LineReader { fd, seekable }
    }

    /// The next line including its newline, or None at end of input.
    pub fn read_line(&mut self) -> Option<String> {
        let mut bytes = Vec::new();
        if self.seekable {
            let mut buf = [0u8; 4096];
            loop {
                let n = unsafe {
                    libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if n < 0 {
                    if errno() == libc::EINTR {
                        continue;
                    }
                    break;
                }
                if n == 0 {
                    break;
                }
                let n = n as usize;
                if let Some(pos) = buf[..n].iter().position(|&b| b == b'\n') {
                    bytes.extend_from_slice(&buf[..=pos]);
                    let back = (n - pos - 1) as libc::off_t;
                    if back > 0 {
                        unsafe {
                            libc::lseek(self.fd, -back, libc::SEEK_CUR);
                        }
                    }
                    break;
                }
                bytes.extend_from_slice(&buf[..n]);
            }
        } else {
            while let Ok(Some(b)) = read_byte(self.fd) {
                bytes.push(b);
                if b == b'\n' {
                    break;
                }
            }
        }
        if bytes.is_empty() {
            None
        } else {
            Some(crate::shell::bytes_to_zstr(&bytes))
        }
    }
}

/// Creates an unlinked temporary file holding `data`, positioned at the
/// start. Used for here-documents so large bodies cannot fill a pipe.
pub fn temp_file_with(data: &[u8]) -> io::Result<i32> {
    let dir = std::env::var("TMPDIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    let template = format!("{}/meowsh-hdXXXXXX", dir.trim_end_matches('/'));
    let mut buf: Vec<u8> = template.into_bytes();
    buf.push(0);
    let fd = unsafe { libc::mkstemp(buf.as_mut_ptr() as *mut libc::c_char) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        libc::unlink(buf.as_ptr() as *const libc::c_char);
    }
    if let Err(e) = write_fd(fd, data) {
        close(fd);
        return Err(e);
    }
    unsafe {
        libc::lseek(fd, 0, libc::SEEK_SET);
    }
    Ok(fd)
}

/// Runs `f` with standard error sent to /dev/null.
pub fn with_stderr_silenced<R>(f: impl FnOnce() -> R) -> R {
    struct RestoreStderr(i32);
    impl Drop for RestoreStderr {
        fn drop(&mut self) {
            if self.0 >= 0 {
                unsafe {
                    libc::dup2(self.0, 2);
                    libc::close(self.0);
                }
            }
        }
    }
    let saved = unsafe { libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 10) };
    let _guard = RestoreStderr(saved);
    let null = std::fs::OpenOptions::new().write(true).open("/dev/null");
    if let Ok(n) = &null {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::dup2(n.as_raw_fd(), 2);
        }
    }
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_reader_leaves_rest_of_seekable_input() {
        let fd = temp_file_with(b"one\ntwo\nthree").unwrap();
        let mut r = LineReader::new(fd);
        assert_eq!(r.read_line().as_deref(), Some("one\n"));
        let rest = String::from_utf8(read_all(fd)).unwrap();
        assert_eq!(rest, "two\nthree");
        close(fd);
    }

    #[test]
    fn line_reader_reads_pipe_bytewise() {
        let (r, w) = pipe_cloexec().unwrap();
        write_fd(w, b"a\nb\n").unwrap();
        close(w);
        let mut lr = LineReader::new(r);
        assert_eq!(lr.read_line().as_deref(), Some("a\n"));
        assert_eq!(String::from_utf8(read_all(r)).unwrap(), "b\n");
        close(r);
    }
}
