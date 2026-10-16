//! Controlled descriptor custody for Linux launcher regressions.
use std::{
    io::{Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
    time::Duration,
};

pub(crate) struct WriterHolder {
    pid: libc::pid_t,
    pipe: UnixStream,
}
impl Drop for WriterHolder {
    fn drop(&mut self) {
        let _ = self.pipe.write_all(&[1]);
        let mut status = 0;
        loop {
            let result = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            if result >= 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
        }
    }
}
impl WriterHolder {
    pub(crate) fn hold(path: &Path) -> Self {
        let writer = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        let writer_fd = writer.as_raw_fd();
        let flags = unsafe { libc::fcntl(writer_fd, libc::F_GETFD) };
        assert!(
            flags >= 0 && flags & libc::FD_CLOEXEC != 0,
            "CLOEXEC must not hide the inherited-writer case"
        );
        let (parent, child) = UnixStream::pair().unwrap();
        let child_fd = child.as_raw_fd();
        let (mut unrelated_reader, unrelated_writer) = UnixStream::pair().unwrap();
        let fd_limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
        assert!(fd_limit > 0);
        let low = writer_fd.min(child_fd) as u32;
        let high = writer_fd.max(child_fd) as u32;
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "controlled fork must start");
        if pid == 0 {
            // Only async-signal-safe syscalls after fork; no Rust teardown.
            unsafe {
                // Do not retain sibling tests' writable launchers or pipe ends.
                for (start, end) in [(0, low), (low + 1, high), (high + 1, u32::MAX)] {
                    if start < end && libc::syscall(libc::SYS_close_range, start, end - 1, 0) != 0 {
                        // Older kernels lack close_range; retain the same isolation.
                        for fd in start..end.min(fd_limit as u32) {
                            libc::close(fd as libc::c_int);
                        }
                    }
                }
                let ready = [1_u8];
                if libc::write(child_fd, ready.as_ptr().cast(), 1) != 1 {
                    libc::_exit(2);
                }
                let mut release = [0_u8];
                libc::read(child_fd, release.as_mut_ptr().cast(), 1);
                libc::_exit(0);
            }
        }
        let mut holder = WriterHolder { pid, pipe: parent };
        drop(child);
        drop(writer); // The parent has closed its writer, like atomic_write does.
        holder
            .pipe
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut ready = [0];
        holder.pipe.read_exact(&mut ready).unwrap();
        assert_eq!(ready, [1]);
        drop(unrelated_writer);
        unrelated_reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(
            unrelated_reader.read(&mut [0]).unwrap(),
            0,
            "the holder must not keep unrelated test descriptors alive"
        );
        holder
    }
}
