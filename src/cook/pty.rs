use libc::{self, winsize};
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::{io::AsRawFd, io::RawFd, process::CommandExt};
use std::process::{Child, Command};
use std::{fs::File, mem, time::Duration};

use crate::{Error, Result, wrap_io_err};

macro_rules! log_to_pty {
    ($logger:expr, $($arg:tt)+) => {
        if $logger.is_some() {
            use std::io::Write;
            let mut logfd = $logger.as_ref().unwrap().1.try_clone().unwrap();
            let _ = logfd.write(format!($($arg)+).as_bytes());
            let _ = logfd.write(&[b'\n']);
        } else {
            eprintln!($($arg)+);
        }
    };
}

pub(crate) use log_to_pty;

pub type PtyOut<'a> = Option<(&'a mut UnixSlavePty, &'a mut PipeWriter)>;

/// The reading end of a pty.
pub type PtyReader = Box<dyn Read + Send>;
/// The writing end of a pty, for feeding input to the process attached to it.
pub type PtyWriter = Box<dyn Write + Send>;
/// What a process writes to: its pty and the pipe collecting cookbook logs.
pub type PtyPipes = (UnixSlavePty, PipeWriter);

/// Setup pty without stdin.
/// Returns (pty stdout, log out, pty+log writer)
pub fn setup_pty() -> (PtyReader, PipeReader, PtyPipes) {
    let pty_system = UnixPtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24, // Standard terminal size
            cols: 80, // Standard terminal size
            ..Default::default()
        })
        .expect("Unable to open pty");

    let pty_reader = pair
        .master
        .try_clone_reader()
        .expect("Unable to clone pty reader");

    let (log_reader, log_writer) = std::io::pipe().expect("Failed to create log pipe");
    let pipes = (pair.slave, log_writer);
    (pty_reader, log_reader, pipes)
}

/// Setup pty with stdin.
/// Returns (pty stdout, log out, pty stdin, pty+log writer)
pub fn setup_pty_with_stdin() -> (PtyReader, PipeReader, PtyWriter, PtyPipes) {
    let pty_system = UnixPtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("Unable to open pty");

    let pty_reader = pair
        .master
        .try_clone_reader()
        .expect("Unable to clone pty reader");

    let pty_writer = pair
        .master
        .try_clone_writer()
        .expect("Unable to clone pty writer");

    let (log_reader, log_writer) = std::io::pipe().expect("Failed to create log pipe");

    let pipes = (pair.slave, log_writer);

    (pty_reader, log_reader, pty_writer, pipes)
}

pub fn flush_pty(logger: &mut PtyOut) {
    let Some((pty, file)) = logger else {
        return;
    };
    // Not sure if flush actually working
    let _ = pty.flush();
    std::thread::sleep(Duration::from_millis(10));
    let _ = file.flush();
}

pub fn spawn_to_pipe(command: &mut Command, stdout_pipe: &PtyOut) -> Result<Child> {
    match stdout_pipe {
        Some(stdout) => stdout.0.spawn_command(command),
        None => Ok(command.spawn().map_err(wrap_io_err!("Spawning"))?),
    }
}

pub fn spawn_to_pipe_with_stdin(command: &mut Command, stdout_pipe: &PtyOut) -> Result<Child> {
    match stdout_pipe {
        Some(stdout) => stdout.0.spawn_command_stdin(command),
        None => Ok(command.spawn().map_err(wrap_io_err!("Spawning"))?),
    }
}

pub fn write_to_pty(pty: &PtyOut, text: &str) {
    log_to_pty!(pty, "{}", text);
}

//
// based on portable-pty crate
// copied here since it isn't flexible enough
//

#[derive(Default)]
pub struct UnixPtySystem {}

/// Represents the size of the visible display area in the pty
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    /// The number of lines of text
    pub rows: u16,
    /// The number of columns of text
    pub cols: u16,
    /// The width of a cell in pixels.  Note that some systems never
    /// fill this value and ignore it.
    pub pixel_width: u16,
    /// The height of a cell in pixels.  Note that some systems never
    /// fill this value and ignore it.
    pub pixel_height: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// `ptsname` returns a pointer into static storage, so calls must not overlap.
static PTSNAME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Open a pseudo-terminal pair with the POSIX interfaces (`posix_openpt`,
/// `grantpt`, `unlockpt`, `ptsname`), which unlike `openpty` are part of the
/// standard and need no extra library.
fn openpty(size: PtySize) -> Result<(UnixMasterPty, UnixSlavePty)> {
    let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_fd < 0 {
        return Err(Error::from_last_io_error("Opening pty master"));
    }
    // Owned from here on, so it is closed on every error path below.
    let master = UnixMasterPty {
        fd: PtyFd(unsafe { File::from_raw_fd(master_fd) }),
    };
    if unsafe { libc::grantpt(master_fd) } != 0 {
        return Err(Error::from_last_io_error("Granting pty"));
    }
    if unsafe { libc::unlockpt(master_fd) } != 0 {
        return Err(Error::from_last_io_error("Unlocking pty"));
    }

    let slave_path = {
        let _guard = PTSNAME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let name = unsafe { libc::ptsname(master_fd) };
        if name.is_null() {
            return Err(Error::from_last_io_error("Getting pty slave name"));
        }
        unsafe { std::ffi::CStr::from_ptr(name) }.to_owned()
    };
    let slave_fd = unsafe { libc::open(slave_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave_fd < 0 {
        return Err(Error::from_last_io_error("Opening pty slave"));
    }
    let slave = UnixSlavePty {
        fd: PtyFd(unsafe { File::from_raw_fd(slave_fd) }),
    };

    master.fd.resize(size)?;

    // Ensure that these descriptors will get closed when we execute
    // the child process.
    cloexec(master.fd.as_raw_fd())?;
    cloexec(slave.fd.as_raw_fd())?;

    Ok((master, slave))
}

pub struct PtyPair {
    // slave is listed first so that it is dropped first.
    // The drop order is stable and specified by rust rfc 1857
    pub slave: UnixSlavePty,
    pub master: UnixMasterPty,
}

impl UnixPtySystem {
    fn openpty(&self, size: PtySize) -> Result<PtyPair> {
        let (master, slave) = openpty(size)?;
        Ok(PtyPair { master, slave })
    }
}

struct PtyFd(pub File);
impl std::ops::Deref for PtyFd {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}

impl Read for PtyFd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buf) {
            Err(ref e) if e.raw_os_error() == Some(libc::EIO) => {
                // EIO indicates that the slave pty has been closed.
                // Treat this as EOF so that std::io::Read::read_to_string
                // and similar functions gracefully terminate when they
                // encounter this condition
                Ok(0)
            }
            x => x,
        }
    }
}

impl Write for PtyFd {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl PtyFd {
    fn resize(&self, size: PtySize) -> Result<()> {
        let ws_size = winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };

        if unsafe {
            libc::ioctl(
                self.0.as_raw_fd(),
                libc::TIOCSWINSZ as _,
                &ws_size as *const _,
            )
        } != 0
        {
            return Err(Error::from_last_io_error("ioctl resize (TIOCSWINSZ)"));
        }

        Ok(())
    }

    fn get_size(&self) -> Result<PtySize> {
        let mut size: winsize = unsafe { mem::zeroed() };
        if unsafe {
            libc::ioctl(
                self.0.as_raw_fd(),
                libc::TIOCGWINSZ as _,
                &mut size as *mut _,
            )
        } != 0
        {
            return Err(Error::from_last_io_error("ioctl get size (TIOCGWINSZ)"));
        }
        Ok(PtySize {
            rows: size.ws_row,
            cols: size.ws_col,
            pixel_width: size.ws_xpixel,
            pixel_height: size.ws_ypixel,
        })
    }

    fn spawn_command(&self, cmd: &mut Command, stdin: bool) -> Result<std::process::Child> {
        cmd.stdout(self.try_clone().map_err(wrap_io_err!("Cloning pty"))?)
            .stderr(self.try_clone().map_err(wrap_io_err!("Cloning pty"))?);

        if stdin {
            cmd.stdin(self.try_clone().map_err(wrap_io_err!("Cloning pty"))?);
        }

        unsafe {
            cmd.pre_exec(move || {
                // Clean up a few things before we exec the program
                // Clear out any potentially problematic signal
                // dispositions that we might have inherited
                for signo in &[
                    libc::SIGCHLD,
                    libc::SIGHUP,
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTERM,
                    libc::SIGALRM,
                ] {
                    libc::signal(*signo, libc::SIG_DFL);
                }

                let empty_set: libc::sigset_t = std::mem::zeroed();
                libc::sigprocmask(libc::SIG_SETMASK, &empty_set, std::ptr::null_mut());

                // Establish ourselves as a session leader.
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }

                Ok(())
            })
        };

        let mut child = cmd.spawn().map_err(wrap_io_err!("Spawning cmd"))?;

        // Ensure that we close out the slave fds that Child retains;
        // they are not what we need (we need the master side to reference
        // them) and won't work in the usual way anyway.
        // In practice these are None, but it seems best to be move them
        // out in case the behavior of Command changes in the future.
        if stdin {
            child.stdin.take();
        }
        child.stdout.take();
        child.stderr.take();

        Ok(child)
    }

    fn flush(&mut self) -> Result<()> {
        self.0.flush().map_err(wrap_io_err!("Flushing pty"))
    }
}

/// Represents the master end of a pty.
/// The file descriptor will be closed when the Pty is dropped.
pub struct UnixMasterPty {
    fd: PtyFd,
}

/// Represents the slave end of a pty.
/// The file descriptor will be closed when the Pty is dropped.
pub struct UnixSlavePty {
    fd: PtyFd,
}

/// Helper function to set the close-on-exec flag for a raw descriptor
fn cloexec(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(Error::from_last_io_error("fcntl to read flags"));
    }
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if result == -1 {
        return Err(Error::from_last_io_error("fcntl to set CLOEXEC"));
    }
    Ok(())
}

impl UnixSlavePty {
    fn spawn_command(&self, builder: &mut Command) -> Result<std::process::Child> {
        self.fd.spawn_command(builder, false)
    }
    fn spawn_command_stdin(&self, builder: &mut Command) -> Result<std::process::Child> {
        self.fd.spawn_command(builder, true)
    }
    fn flush(&mut self) -> Result<()> {
        self.fd.flush()
    }
}

impl UnixMasterPty {
    #[allow(unused)]
    fn resize(&self, size: PtySize) -> Result<()> {
        self.fd.resize(size)
    }

    #[allow(unused)]
    fn get_size(&self) -> Result<PtySize> {
        self.fd.get_size()
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
        let fd = PtyFd(
            self.fd
                .try_clone()
                .map_err(wrap_io_err!("Cloning pty fd"))?,
        );
        Ok(Box::new(fd))
    }

    pub fn try_clone_writer(&self) -> Result<Box<dyn Write + Send>> {
        let fd = PtyFd(
            self.fd
                .try_clone()
                .map_err(wrap_io_err!("Cloning pty fd"))?,
        );
        Ok(Box::new(fd))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_pty_pair_carries_data_and_size() {
        let size = PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        };
        let (master, mut slave) = openpty(size).unwrap();
        assert_eq!(master.get_size().unwrap(), size);

        slave.fd.write_all(b"ping\n").unwrap();
        let mut reader = master.try_clone_reader().unwrap();
        let mut buf = [0u8; 16];
        let n = reader.read(&mut buf).unwrap();
        // The line discipline may turn "\n" into "\r\n".
        assert!(buf[..n].starts_with(b"ping"), "{:?}", &buf[..n]);
    }
}
