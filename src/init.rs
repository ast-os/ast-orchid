//! PID 1: bring the system up, keep the services running, answer `orchid`, take it all down.
//!
//! One loop waits for signals (SIGCHLD above all) and for the control socket. Starting and
//! stopping never wait inside it: every service has a state, a request only changes what is
//! wanted, and `advance` moves each service as far as its dependencies allow. A control command
//! that waits for a result is kept in `waiters` and answered when the services it names settle.

use crate::console;
use crate::service::{self, Kind, Output, Restart, Service};
use crate::sys::{self, Halt, SignalFd};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const SERVICES: &str = "/etc/orchid/services";
pub const ENABLED: &str = "/etc/orchid/enabled";
pub const SOCKET: &str = "/run/orchid.sock";
pub const LOGS: &str = "/var/log/orchid";
const RESTARTS: usize = 5;
const WINDOW: Duration = Duration::from_secs(30);
const ONESHOT_LIMIT: Duration = Duration::from_secs(300);
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(30);
/// A log file is rotated when it would grow past this; this many old ones are kept.
const LOG_LIMIT: u64 = 1 << 20;
const LOG_KEEP: u32 = 5;
const HOOK_LIMIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    /// Not running; with `want` set it is waiting for what it depends on.
    Idle,
    /// A oneshot is running.
    Starting,
    Up,
    /// SIGTERM was sent.
    Stopping,
    /// A oneshot finished.
    Done,
    Failed,
}

impl State {
    fn word(self) -> &'static str {
        match self {
            State::Idle => "stopped",
            State::Starting => "starting",
            State::Up => "up",
            State::Stopping => "stopping",
            State::Done => "done",
            State::Failed => "failed",
        }
    }
}

struct Unit {
    svc: Service,
    pid: Option<i32>,
    state: State,
    /// It should be running (or have run): asked for at boot or by `start`, or needed by one that is.
    want: bool,
    since: Option<Instant>,
    exit: Option<i32>,
    why: Option<String>,
    /// A oneshot that runs too long, or a service that ignores SIGTERM, is killed then.
    deadline: Option<Instant>,
    restarts: Vec<Instant>,
    total_restarts: u32,
    restart_at: Option<Instant>,
    /// What the service prints, on its way to the log or the console.
    logger: Option<Logger>,
}

impl Unit {
    fn new(svc: Service) -> Unit {
        Unit { svc, pid: None, state: State::Idle, want: false, since: None, exit: None, why: None, deadline: None, restarts: vec![], total_restarts: 0, restart_at: None, logger: None }
    }
}

enum Sink {
    File { name: String, file: Option<File>, size: u64 },
    Console,
}

/// The read end of a service's output: lines are written to the log, with the time, or to the
/// consoles. A log that gets too big is rotated; nothing is lost between the files.
struct Logger {
    fd: OwnedFd,
    partial: Vec<u8>,
    sink: Sink,
}

impl Logger {
    /// Read what is there. False once every writer has gone.
    fn pump(&mut self) -> bool {
        use std::os::fd::AsRawFd;
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: the buffer is valid for its length
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                self.partial.extend_from_slice(&buf[..n as usize]);
                self.lines();
            } else if n == 0 {
                self.flush();
                return false;
            } else {
                match std::io::Error::last_os_error().kind() {
                    std::io::ErrorKind::WouldBlock => return true,
                    std::io::ErrorKind::Interrupted => {}
                    _ => return false,
                }
            }
        }
    }

    fn lines(&mut self) {
        while let Some(i) = self.partial.iter().position(|&b| b == b'\n').or((self.partial.len() > 16384).then_some(self.partial.len() - 1)) {
            let raw: Vec<u8> = self.partial.drain(..=i).collect();
            let text = String::from_utf8_lossy(&raw);
            self.emit(text.trim_end_matches('\n'));
        }
    }

    fn flush(&mut self) {
        if !self.partial.is_empty() {
            let raw = std::mem::take(&mut self.partial);
            self.emit(&String::from_utf8_lossy(&raw));
        }
    }

    fn emit(&mut self, line: &str) {
        match &mut self.sink {
            Sink::Console => console::write(&format!("{line}\n")),
            Sink::File { name, file, size } => {
                let text = format!("{} {line}\n", stamp());
                if *size + text.len() as u64 > LOG_LIMIT {
                    rotate(name);
                    *file = None;
                    *size = 0;
                }
                if file.is_none() {
                    *file = open_log(name);
                    *size = file.as_ref().and_then(|f| f.metadata().ok()).map_or(0, |m| m.len());
                }
                if let Some(f) = file {
                    let _ = f.write_all(text.as_bytes());
                    *size += text.len() as u64;
                }
            }
        }
    }
}

fn open_log(name: &str) -> Option<File> {
    let _ = std::fs::create_dir_all(LOGS);
    std::fs::OpenOptions::new().create(true).append(true).open(format!("{LOGS}/{name}.log")).ok()
}

/// NAME.log becomes NAME.log.1, that NAME.log.2, and so on; the oldest goes.
fn rotate(name: &str) {
    for i in (1..LOG_KEEP).rev() {
        let _ = std::fs::rename(format!("{LOGS}/{name}.log.{i}"), format!("{LOGS}/{name}.log.{}", i + 1));
    }
    let _ = std::fs::rename(format!("{LOGS}/{name}.log"), format!("{LOGS}/{name}.log.1"));
}

/// `YYYY-MM-DD HH:MM:SS`, local time.
fn stamp() -> String {
    // SAFETY: tm and the buffer are plain data, strftime writes at most the buffer's length
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        let mut buf = [0u8; 32];
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), c"%Y-%m-%d %H:%M:%S".as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

enum Job {
    Start,
    Stop,
    /// Stopping first; when that is done the services are started.
    Restart(bool),
}

/// A control command that is answered when the services it names have settled.
struct Waiter {
    stream: UnixStream,
    job: Job,
    names: Vec<String>,
}

enum Deps {
    Ready,
    Wait,
    Failed(String),
}

struct Supervisor {
    units: Vec<Unit>,
    halt: Option<Halt>,
    waiters: Vec<Waiter>,
    /// The services have control groups.
    cg: bool,
    /// `normal`, or `rescue` when the kernel command line says so.
    mode: &'static str,
}

pub fn run() -> ! {
    let signals = SignalFd::new();
    // the consoles are listed in /sys and opened in /dev, so the file systems come first; what they
    // have to report is shown after the banner, on every console
    let mounted = mount_basics();
    console::quiet_kernel();
    console::open();
    console::clear();
    console::banner();
    console::title(&format!("orchid {}", env!("CARGO_PKG_VERSION")));
    for (marker, text) in mounted {
        if marker == '+' {
            console::ok(&text);
        } else {
            console::warn(&text);
        }
    }
    let cg = sys::cgroup::init();
    clean_tmp();
    hostname();
    let (services, mut problems) = service::load(Path::new(SERVICES));
    let (ordered, more) = service::order(services);
    problems.extend(more);
    for p in problems {
        console::warn(&p);
    }
    let mode = boot_mode();
    if mode != "normal" {
        console::title(&format!("{mode} mode"));
    }
    let mut sup = Supervisor { units: ordered.into_iter().map(Unit::new).collect(), halt: None, waiters: vec![], cg, mode };
    let listener = bind();
    sup.boot();
    let how = sup.serve(&signals, listener.as_ref());
    sup.shutdown(how)
}

/// `rescue` for `single`, `rescue` or `orchid.mode=rescue` on the kernel command line.
fn boot_mode() -> &'static str {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if cmdline.split_whitespace().any(|w| matches!(w, "single" | "rescue" | "orchid.mode=rescue")) { "rescue" } else { "normal" }
}

/// Mount what a running system needs; what to say about it is returned.
fn mount_basics() -> Vec<(char, String)> {
    const FLAGS: u64 = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    let table: [(&str, &str, &str, u64, &str); 8] = [
        ("proc", "/proc", "proc", FLAGS, ""),
        ("sysfs", "/sys", "sysfs", FLAGS, ""),
        ("cgroup2", "/sys/fs/cgroup", "cgroup2", FLAGS, ""),
        ("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID, "mode=0755"),
        ("devpts", "/dev/pts", "devpts", libc::MS_NOSUID | libc::MS_NOEXEC, "gid=5,mode=0620"),
        ("tmpfs", "/dev/shm", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, "mode=1777"),
        ("tmpfs", "/run", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, "mode=0755"),
        ("tmpfs", "/tmp", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, "mode=1777"),
    ];
    let mut done = Vec::new();
    let mut said = Vec::new();
    for (source, target, fstype, flags, data) in table {
        if sys::is_mounted(target) {
            continue;
        }
        if sys::mount(source, target, fstype, flags, data) {
            if !target[1..].contains('/') && target != "/run" {
                done.push(target);
            }
        } else {
            said.push(('!', format!("unable to mount {target}")));
        }
    }
    done.sort_by_key(|t| ["/sys", "/dev", "/proc", "/tmp"].iter().position(|x| x == t).unwrap_or(9));
    if !done.is_empty() {
        said.insert(0, ('+', format!("mounting {}", done.join(" "))));
    }
    said
}

fn clean_tmp() {
    if let Ok(dir) = std::fs::read_dir("/tmp") {
        for e in dir.flatten() {
            let p = e.path();
            let _ = if p.is_dir() && !p.is_symlink() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
        }
    }
    console::ok("cleaning /tmp");
}

fn hostname() {
    if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
        let name = name.trim();
        if !name.is_empty() {
            if sys::set_hostname(name) {
                console::ok(&format!("hostname {name}"));
            } else {
                console::warn("unable to set the hostname");
            }
        }
    }
}

/// The control socket; anyone may connect, only root may change anything.
fn bind() -> Option<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(SOCKET);
    let l = UnixListener::bind(SOCKET).ok()?;
    let _ = std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o666));
    l.set_nonblocking(true).ok()?;
    Some(l)
}

/// Start a service: in its own session and control group, with its output on a pipe that
/// `Logger` reads. A service that is to talk to a person gets the console as its terminal.
fn spawn(svc: &Service, cg: bool) -> Result<(i32, Option<Logger>), String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let mut c = Command::new(&svc.exec[0]);
    c.args(&svc.exec[1..]);
    for (k, v) in &svc.env {
        c.env(k, v);
    }
    if let Some(w) = &svc.workdir {
        c.current_dir(w);
    }
    let (uid, gid) = sys::ids(svc.user.as_deref(), svc.group.as_deref())?;
    let tty = svc.output == Output::Tty;
    let mut logger = None;
    if tty {
        let con = std::fs::OpenOptions::new().read(true).write(true).open("/dev/console").map_err(|e| format!("cannot open /dev/console: {e}"))?;
        let (out, err) = (con.try_clone().map_err(|e| e.to_string())?, con.try_clone().map_err(|e| e.to_string())?);
        c.stdin(con).stdout(out).stderr(err);
    } else {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` has room for the two descriptors
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err("cannot make a pipe".into());
        }
        // SAFETY: both descriptors were just made and belong to nobody else
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        // SAFETY: plain flag change on the read end
        unsafe {
            let flags = libc::fcntl(read.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(read.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let write2 = write.try_clone().map_err(|e| e.to_string())?;
        c.stdin(Stdio::null()).stdout(Stdio::from(write)).stderr(Stdio::from(write2));
        let sink = if svc.output == Output::File {
            let file = open_log(&svc.name);
            let size = file.as_ref().and_then(|f| f.metadata().ok()).map_or(0, |m| m.len());
            Sink::File { name: svc.name.clone(), file, size }
        } else {
            Sink::Console
        };
        logger = Some(Logger { fd: read, partial: Vec::new(), sink });
    }
    let procs = if cg { sys::cgroup::create(&svc.name).and_then(|p| std::ffi::CString::new(p.as_os_str().as_bytes()).ok()) } else { None };
    // SAFETY: only async-signal-safe calls run between fork and exec, and nothing allocates. The
    // process joins its control group while it is still root, then becomes the service's user;
    // the signals orchid blocks for itself must not stay blocked in what it starts.
    unsafe {
        c.pre_exec(move || {
            libc::setsid();
            if tty {
                libc::ioctl(0, libc::TIOCSCTTY as _, 1);
            }
            if let Some(p) = &procs {
                let fd = libc::open(p.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if fd >= 0 {
                    libc::write(fd, b"0\n".as_ptr().cast(), 2);
                    libc::close(fd);
                }
            }
            if uid.is_some() || gid.is_some() {
                libc::setgroups(0, std::ptr::null());
                if let Some(g) = gid
                    && libc::setgid(g) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(u) = uid
                    && libc::setuid(u) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            Ok(())
        });
    }
    let child = c.spawn().map_err(|e| format!("cannot run {}: {e}", svc.exec[0]))?;
    let pid = child.id() as i32;
    std::mem::forget(child); // reaped by the supervisor, not by Child
    Ok((pid, logger))
}

/// Run what a service says to run when it stops; it gets a few seconds.
fn run_stop_hook(svc: &Service) {
    if svc.stop.is_empty() {
        return;
    }
    let mut c = Command::new(&svc.stop[0]);
    c.args(&svc.stop[1..]).stdin(Stdio::null());
    for (k, v) in &svc.env {
        c.env(k, v);
    }
    // SAFETY: sigprocmask is async-signal-safe; the signals orchid blocks must not stay blocked
    unsafe {
        c.pre_exec(|| {
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            Ok(())
        });
    }
    match c.spawn() {
        Ok(mut child) => {
            let end = Instant::now() + HOOK_LIMIT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        Err(e) => console::warn(&format!("{}: cannot run {}: {e}", svc.name, svc.stop[0])),
    }
}

/// A signal by number or name (`HUP`, `SIGUSR1`, ...).
fn signal_number(s: &str) -> Result<i32, String> {
    if let Ok(n) = s.parse::<i32>() {
        return Ok(n);
    }
    let name = s.trim_start_matches("SIG");
    [("HUP", libc::SIGHUP), ("INT", libc::SIGINT), ("QUIT", libc::SIGQUIT), ("KILL", libc::SIGKILL), ("USR1", libc::SIGUSR1), ("USR2", libc::SIGUSR2), ("TERM", libc::SIGTERM), ("CONT", libc::SIGCONT), ("STOP", libc::SIGSTOP)]
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| *v)
        .ok_or_else(|| format!("no signal {s:?}"))
}

fn age(since: Option<Instant>) -> String {
    let s = since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

impl Supervisor {
    fn index(&self, name: &str) -> Option<usize> {
        self.units.iter().position(|u| u.svc.name == name)
    }

    fn boot(&mut self) {
        let enabled = service::enabled(Path::new(ENABLED));
        for i in 0..self.units.len() {
            if enabled.as_ref().is_none_or(|e| e.contains(&self.units[i].svc.name)) && self.in_mode(i) {
                self.want_up(i);
            }
        }
        self.advance();
    }

    /// Does the service belong to the mode the system booted in?
    fn in_mode(&self, i: usize) -> bool {
        self.units[i].svc.modes.iter().any(|m| m == self.mode)
    }

    /// Ask for a service and, first, for what it needs and wants.
    fn want_up(&mut self, i: usize) {
        if self.units[i].want && self.units[i].state != State::Failed {
            return;
        }
        let u = &mut self.units[i];
        u.want = true;
        if u.state == State::Failed {
            u.state = State::Idle;
            u.why = None;
        }
        let deps: Vec<String> = u.svc.needs.iter().chain(&u.svc.wants).cloned().collect();
        for n in deps {
            if let Some(j) = self.index(&n) {
                self.want_up(j);
            }
        }
    }

    /// Stop asking for a service, and for what needs it.
    fn want_down(&mut self, i: usize) {
        let name = self.units[i].svc.name.clone();
        let u = &mut self.units[i];
        u.want = false;
        u.restart_at = None;
        if u.state == State::Done {
            run_stop_hook(&u.svc);
            if self.cg {
                sys::cgroup::clear(&name);
            }
        }
        let u = &mut self.units[i];
        if matches!(u.state, State::Done | State::Failed) {
            u.state = State::Idle;
        }
        for j in 0..self.units.len() {
            if j != i && self.units[j].want && self.units[j].svc.needs.contains(&name) {
                self.want_down(j);
            }
        }
    }

    fn deps(&self, i: usize) -> Deps {
        let svc = &self.units[i].svc;
        for n in &svc.needs {
            let Some(j) = self.index(n) else { return Deps::Failed(format!("needs {n}, which does not exist")) };
            match self.units[j].state {
                State::Up | State::Done => {}
                State::Failed => return Deps::Failed(format!("needs {n}, which failed")),
                _ => return Deps::Wait,
            }
        }
        for n in svc.wants.iter().chain(&svc.after) {
            if let Some(j) = self.index(n)
                && self.units[j].want
                && matches!(self.units[j].state, State::Idle | State::Starting | State::Stopping)
            {
                return Deps::Wait;
            }
        }
        Deps::Ready
    }

    /// Move every service as far as it can go now. Runs after anything that changed a state.
    fn advance(&mut self) {
        loop {
            let mut changed = false;
            for i in 0..self.units.len() {
                match (self.units[i].want, self.units[i].state) {
                    (true, State::Idle) if self.units[i].restart_at.is_none() => match self.deps(i) {
                        Deps::Ready => {
                            self.spawn_unit(i);
                            changed = true;
                        }
                        Deps::Failed(why) => {
                            let u = &mut self.units[i];
                            u.state = State::Failed;
                            console::warn(&format!("{} {why}", u.svc.name));
                            u.why = Some(why);
                            changed = true;
                        }
                        Deps::Wait => {}
                    },
                    (false, State::Up | State::Starting) => {
                        let name = &self.units[i].svc.name;
                        let busy = self.units.iter().any(|d| d.svc.needs.contains(name) && matches!(d.state, State::Up | State::Starting | State::Stopping));
                        if !busy {
                            self.terminate(i);
                            changed = true;
                        }
                    }
                    _ => {}
                }
            }
            if !changed {
                return;
            }
        }
    }

    fn spawn_unit(&mut self, i: usize) {
        let (pid, logger) = match spawn(&self.units[i].svc, self.cg) {
            Ok(r) => r,
            Err(e) => {
                let u = &mut self.units[i];
                u.state = State::Failed;
                console::warn(&format!("{} failed: {e}", u.svc.name));
                u.why = Some(e);
                return;
            }
        };
        let u = &mut self.units[i];
        u.pid = Some(pid);
        u.logger = logger;
        u.since = Some(Instant::now());
        u.exit = None;
        u.why = None;
        if u.svc.kind == Kind::Daemon {
            u.state = State::Up;
            console::ok(&format!("started {}", u.svc.name));
        } else {
            u.state = State::Starting;
            u.deadline = Some(Instant::now() + ONESHOT_LIMIT);
        }
    }

    /// Ask a service to stop: its hook first, then SIGTERM to everything in its control group.
    fn terminate(&mut self, i: usize) {
        run_stop_hook(&self.units[i].svc);
        self.signal_unit(i, libc::SIGTERM);
        let (cg, u) = (self.cg, &mut self.units[i]);
        if u.pid.is_some() || (cg && sys::cgroup::populated(&u.svc.name)) {
            u.state = State::Stopping;
            u.deadline = Some(Instant::now() + Duration::from_secs(u.svc.timeout));
        } else {
            u.state = State::Idle;
        }
    }

    /// A signal for the whole service: every process of its control group, or its main process.
    fn signal_unit(&self, i: usize, sig: i32) {
        let u = &self.units[i];
        if self.cg {
            sys::cgroup::signal(&u.svc.name, sig);
        } else if let Some(pid) = u.pid {
            sys::kill(pid, sig);
        }
    }

    /// A child ended: a service is restarted if its policy says so; anything else was an orphan.
    fn exited(&mut self, pid: i32, code: i32) {
        let Some(i) = self.units.iter().position(|u| u.pid == Some(pid)) else { return };
        let cg = self.cg;
        let u = &mut self.units[i];
        u.pid = None;
        u.exit = Some(code);
        u.deadline = None;
        let name = u.svc.name.clone();
        match u.state {
            State::Stopping => {
                u.state = State::Idle;
                console::gone(&format!("stopped {name}"));
            }
            State::Starting if code == 0 && u.svc.kind == Kind::Forking => {
                if cg && !sys::cgroup::populated(&name) {
                    u.state = State::Failed;
                    u.why = Some("left nothing running".into());
                    console::warn(&format!("{name} left nothing running"));
                } else {
                    u.state = State::Up;
                    u.since = Some(Instant::now());
                    console::ok(&format!("started {name}"));
                }
            }
            State::Starting if code == 0 => {
                u.state = State::Done;
                console::ok(&format!("ran {name}"));
            }
            State::Starting => {
                u.state = State::Failed;
                u.why = Some(format!("exit {code}"));
                console::warn(&format!("{name} failed (exit {code})"));
            }
            _ => self.daemon_died(i, code),
        }
        // what a service that is not up or done left behind goes with it
        if cg && !matches!(self.units[i].state, State::Up | State::Done) {
            sys::cgroup::clear(&name);
        }
    }

    fn daemon_died(&mut self, i: usize, code: i32) {
        let u = &mut self.units[i];
        u.state = State::Failed;
        u.why = Some(format!("exit {code}"));
        let again = match u.svc.restart {
            Restart::Always => true,
            Restart::OnFailure => code != 0,
            Restart::No => false,
        };
        if !again {
            console::warn(&format!("{} stopped (exit {code})", u.svc.name));
            return;
        }
        let now = Instant::now();
        u.restarts.retain(|t| now.duration_since(*t) < WINDOW);
        if u.restarts.len() >= RESTARTS {
            console::warn(&format!("{} keeps failing, giving up", u.svc.name));
            return;
        }
        u.restarts.push(now);
        u.total_restarts += 1;
        u.restart_at = Some(now + Duration::from_secs(1));
        console::warn(&format!("{} stopped (exit {code}), restarting", u.svc.name));
    }

    /// What time has to say: restarts that are due, services that do not stop, oneshots that do not
    /// end, daemons that left (their control group is empty).
    fn timers(&mut self) {
        let now = Instant::now();
        for i in 0..self.units.len() {
            let name = self.units[i].svc.name.clone();
            let u = &mut self.units[i];
            if u.restart_at.is_some_and(|t| t <= now) {
                u.restart_at = None;
                u.state = State::Idle;
            }
            if u.deadline.is_some_and(|t| t <= now) {
                u.deadline = None;
                if self.cg {
                    sys::cgroup::kill(&name);
                    console::gone(&format!("killed {name}"));
                } else if let Some(pid) = u.pid {
                    sys::kill(pid, libc::SIGKILL);
                    console::gone(&format!("killed {name}"));
                }
            }
            let u = &self.units[i];
            if self.cg && u.svc.kind == Kind::Forking && u.pid.is_none() && matches!(u.state, State::Up | State::Stopping) && !sys::cgroup::populated(&name) {
                if u.state == State::Stopping {
                    let u = &mut self.units[i];
                    u.state = State::Idle;
                    u.deadline = None;
                    console::gone(&format!("stopped {name}"));
                    sys::cgroup::clear(&name);
                } else {
                    self.daemon_died(i, 1);
                    sys::cgroup::clear(&name);
                }
            }
        }
    }

    /// Milliseconds to the next thing in `timers`, at most a second.
    fn next_timer(&self) -> i32 {
        let now = Instant::now();
        let soonest = self.units.iter().flat_map(|u| [u.restart_at, u.deadline]).flatten().min();
        soonest.map_or(1000, |t| t.saturating_duration_since(now).as_millis().clamp(10, 1000) as i32)
    }

    /// Serve until something asks to stop: TERM or INT reboot, USR1 halts, USR2 powers off, or
    /// a command over the socket.
    fn serve(&mut self, signals: &SignalFd, listener: Option<&UnixListener>) -> Halt {
        loop {
            let mut fds = vec![signals.fd()];
            fds.extend(listener.map(|l| l.as_raw_fd()));
            fds.extend(self.units.iter().filter_map(|u| u.logger.as_ref().map(|l| std::os::fd::AsRawFd::as_raw_fd(&l.fd))));
            let ready = sys::poll(&fds, self.next_timer());
            self.pump_logs();
            if ready[0] {
                for s in signals.read() {
                    match s {
                        libc::SIGTERM | libc::SIGINT => return Halt::Reboot,
                        libc::SIGUSR1 => return Halt::Stop,
                        libc::SIGUSR2 => return Halt::Poweroff,
                        _ => {}
                    }
                }
            }
            while let Some((pid, code)) = sys::reap() {
                self.exited(pid, code);
            }
            self.timers();
            if let (Some(l), Some(true)) = (listener, ready.get(1)) {
                while let Ok((stream, _)) = l.accept() {
                    self.client(stream);
                    if let Some(h) = self.halt {
                        return h;
                    }
                }
            }
            self.advance();
            self.answer_waiters();
        }
    }

    /// Move what the services printed to the logs and the consoles.
    fn pump_logs(&mut self) {
        for u in &mut self.units {
            if let Some(l) = &mut u.logger
                && !l.pump()
            {
                u.logger = None;
            }
        }
    }

    fn client(&mut self, mut stream: UnixStream) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let uid = sys::peer_uid(stream.as_raw_fd()).unwrap_or(u32::MAX);
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            return;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let (cmd, args) = match words.split_first() {
            Some(x) => x,
            None => {
                let _ = writeln!(stream, "error empty command");
                return;
            }
        };
        let reply = if !matches!(*cmd, "list" | "status" | "version") && uid != 0 {
            Err("only root may do that".to_string())
        } else {
            self.handle(cmd, args, stream.try_clone().ok())
        };
        match reply {
            Ok(Some(text)) => {
                let _ = write!(stream, "ok\n{text}");
            }
            Ok(None) => {} // answered later, by `answer_waiters`
            Err(e) => {
                let _ = writeln!(stream, "error {e}");
            }
        }
    }

    fn named(&self, cmd: &str, args: &[&str]) -> Result<Vec<usize>, String> {
        if args.is_empty() {
            return Err(format!("{cmd} needs a service name"));
        }
        args.iter().map(|n| self.index(n).ok_or_else(|| format!("no service {n:?}"))).collect()
    }

    fn handle(&mut self, cmd: &str, args: &[&str], stream: Option<UnixStream>) -> Result<Option<String>, String> {
        let now = |s: String| Ok(Some(s));
        match cmd {
            "version" => now(format!("orchid  {}\n", env!("CARGO_PKG_VERSION"))),
            "list" => now(self.list()),
            "status" => {
                let i = self.named(cmd, args)?[0];
                now(self.status(i))
            }
            "start" | "stop" | "restart" => {
                let idx = self.named(cmd, args)?;
                let names: Vec<String> = idx.iter().map(|&i| self.units[i].svc.name.clone()).collect();
                let job = match cmd {
                    "start" => {
                        idx.iter().for_each(|&i| self.want_up(i));
                        Job::Start
                    }
                    "stop" => {
                        idx.iter().for_each(|&i| self.want_down(i));
                        Job::Stop
                    }
                    _ => {
                        idx.iter().for_each(|&i| self.want_down(i));
                        Job::Restart(false)
                    }
                };
                self.advance();
                self.waiters.push(Waiter { stream: stream.ok_or("no connection")?, job, names });
                self.answer_waiters();
                Ok(None)
            }
            "enable" | "disable" => {
                let idx = self.named(cmd, args)?;
                let names: Vec<String> = idx.iter().map(|&i| self.units[i].svc.name.clone()).collect();
                set_enabled(&names, cmd == "enable")?;
                now(String::new())
            }
            "kill" => {
                let i = self.named(cmd, &args[..args.len().min(1)])?[0];
                let sig = args.get(1).map_or(Ok(libc::SIGTERM), |s| signal_number(s))?;
                self.signal_unit(i, sig);
                now(String::new())
            }
            "reload" => now(self.reload()),
            "reboot" | "poweroff" | "halt" => {
                self.halt = Some(match cmd {
                    "reboot" => Halt::Reboot,
                    "poweroff" => Halt::Poweroff,
                    _ => Halt::Stop,
                });
                now(String::new())
            }
            other => Err(format!("unknown command {other:?}")),
        }
    }

    /// Answer the commands whose services have settled.
    fn answer_waiters(&mut self) {
        let mut i = 0;
        while i < self.waiters.len() {
            let result = self.check_waiter(i);
            match result {
                None => i += 1,
                Some(r) => {
                    let mut w = self.waiters.remove(i);
                    let _ = match r {
                        Ok(()) => writeln!(w.stream, "ok"),
                        Err(e) => writeln!(w.stream, "error {e}"),
                    };
                }
            }
        }
    }

    /// `None` while still going; the answer once settled.
    fn check_waiter(&mut self, k: usize) -> Option<Result<(), String>> {
        let names = self.waiters[k].names.clone();
        let idx: Vec<usize> = names.iter().filter_map(|n| self.index(n)).collect();
        let stopped = idx.iter().all(|&i| self.units[i].state == State::Idle && self.units[i].pid.is_none());
        match self.waiters[k].job {
            Job::Stop => stopped.then_some(Ok(())),
            Job::Restart(false) => {
                if !stopped {
                    return None;
                }
                idx.iter().for_each(|&i| self.want_up(i));
                self.waiters[k].job = Job::Restart(true);
                self.advance();
                self.check_waiter(k)
            }
            Job::Start | Job::Restart(true) => {
                let mut pending = false;
                for &i in &idx {
                    let u = &self.units[i];
                    match u.state {
                        State::Up | State::Done => {}
                        State::Failed => return Some(Err(format!("{}: {}", u.svc.name, u.why.clone().unwrap_or_else(|| "failed".into())))),
                        _ => pending = true,
                    }
                }
                (!pending).then_some(Ok(()))
            }
        }
    }

    fn list(&self) -> String {
        let enabled = service::enabled(Path::new(ENABLED));
        let mut out = String::new();
        for u in &self.units {
            let boot = if enabled.as_ref().is_none_or(|e| e.contains(&u.svc.name)) { "on" } else { "off" };
            let detail = match (u.state, u.pid, &u.why) {
                (State::Up, Some(p), _) => format!("pid {p}  {}", age(u.since)),
                (State::Failed, _, Some(w)) => w.clone(),
                _ => String::new(),
            };
            out += &[u.svc.name.as_str(), u.state.word(), boot, detail.as_str()].iter().filter(|c| !c.is_empty()).copied().collect::<Vec<_>>().join("  ");
            out.push('\n');
        }
        out
    }

    fn status(&self, i: usize) -> String {
        let u = &self.units[i];
        let enabled = service::enabled(Path::new(ENABLED));
        let mut rows: Vec<(String, String)> = vec![("name".into(), u.svc.name.clone())];
        if !u.svc.description.is_empty() {
            rows.push(("description".into(), u.svc.description.clone()));
        }
        rows.push(("state".into(), u.state.word().into()));
        if let Some(w) = &u.why {
            rows.push(("why".into(), w.clone()));
        }
        if let Some(p) = u.pid {
            rows.push(("pid".into(), p.to_string()));
            rows.push(("up for".into(), age(u.since)));
        }
        if self.cg && matches!(u.state, State::Up | State::Starting | State::Stopping) {
            rows.push(("processes".into(), sys::cgroup::procs(&u.svc.name).len().to_string()));
        }
        if let Some(c) = u.exit {
            rows.push(("last exit".into(), c.to_string()));
        }
        rows.push(("restarts".into(), u.total_restarts.to_string()));
        rows.push(("boot".into(), if enabled.as_ref().is_none_or(|e| e.contains(&u.svc.name)) { "on" } else { "off" }.into()));
        for (k, v) in [("needs", &u.svc.needs), ("wants", &u.svc.wants), ("after", &u.svc.after)] {
            if !v.is_empty() {
                rows.push((k.into(), v.join(", ")));
            }
        }
        rows.push(("exec".into(), u.svc.exec.join(" ")));
        if u.svc.output == Output::File {
            rows.push(("log".into(), format!("{LOGS}/{}.log", u.svc.name)));
        }
        rows.iter().map(|(k, v)| format!("{k}  {v}\n")).collect()
    }

    /// Read the service files again: new ones appear (and start, if enabled), services that are
    /// not running are redefined, vanished ones go; running ones keep running as they are.
    fn reload(&mut self) -> String {
        let (services, mut problems) = service::load(Path::new(SERVICES));
        let (ordered, more) = service::order(services);
        problems.extend(more);
        let enabled = service::enabled(Path::new(ENABLED));
        let (mut added, mut changed) = (0, 0);
        let mut units: Vec<Unit> = Vec::new();
        for svc in ordered {
            match self.index(&svc.name) {
                Some(i) => {
                    let mut u = Unit::new(svc.clone());
                    std::mem::swap(&mut u, &mut self.units[i]);
                    if !matches!(u.state, State::Up | State::Starting | State::Stopping) {
                        changed += 1;
                        u.svc = svc;
                    }
                    units.push(u);
                }
                None => {
                    added += 1;
                    let mut u = Unit::new(svc);
                    u.want = false;
                    units.push(u);
                }
            }
        }
        // a service that is running and no longer has a file stays until it is stopped
        for u in self.units.drain(..) {
            if matches!(u.state, State::Up | State::Starting | State::Stopping) && !units.iter().any(|x| x.svc.name == u.svc.name) {
                units.push(u);
            }
        }
        self.units = units;
        for i in 0..self.units.len() {
            let u = &self.units[i];
            if u.state == State::Idle && !u.want && u.exit.is_none() && enabled.as_ref().is_none_or(|e| e.contains(&u.svc.name)) && self.in_mode(i) {
                self.want_up(i);
            }
        }
        self.advance();
        let mut out = format!("{added} added  {changed} updated\n");
        for p in problems {
            out += &format!("{p}\n");
        }
        out
    }

    /// Take everything down: services in reverse order of need, then whatever is left.
    fn shutdown(&mut self, how: Halt) -> ! {
        console::title("shutting down");
        for i in 0..self.units.len() {
            self.want_down(i);
        }
        let end = Instant::now() + SHUTDOWN_LIMIT;
        while Instant::now() < end && self.units.iter().any(|u| matches!(u.state, State::Up | State::Starting | State::Stopping)) {
            self.advance();
            while let Some((pid, code)) = sys::reap() {
                self.exited(pid, code);
            }
            self.timers();
            self.pump_logs();
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_file(SOCKET);
        sys::kill(-1, libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(500));
        sys::kill(-1, libc::SIGKILL);
        while sys::reap().is_some() {}
        sys::sync();
        unmount_all();
        sys::sync();
        sys::halt(how)
    }
}

/// Enable or disable services at boot. Without an `enabled` directory every service starts, so
/// the first time it is made from what exists.
fn set_enabled(names: &[String], on: bool) -> Result<(), String> {
    let dir = Path::new(ENABLED);
    if !dir.exists() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {ENABLED}: {e}"))?;
        for e in std::fs::read_dir(SERVICES).map_err(|e| e.to_string())?.flatten() {
            let f = e.file_name().to_string_lossy().into_owned();
            if f.ends_with(".toml") {
                let _ = std::os::unix::fs::symlink(format!("../services/{f}"), dir.join(&f));
            }
        }
    }
    for n in names {
        let link = dir.join(format!("{n}.toml"));
        let _ = std::fs::remove_file(&link);
        if on {
            std::os::unix::fs::symlink(format!("../services/{n}.toml"), &link).map_err(|e| format!("cannot enable {n}: {e}"))?;
        }
    }
    Ok(())
}

/// Unmount everything except the root and the kernel's own file systems, the deepest first.
fn unmount_all() {
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mut points: Vec<String> = info.lines().filter_map(|l| l.split(' ').nth(4)).map(String::from).collect();
    points.sort_by_key(|p| std::cmp::Reverse(p.len()));
    for p in points.iter().filter(|p| !["/", "/proc", "/sys", "/dev"].contains(&p.as_str())) {
        if sys::umount(p) {
            console::gone(&format!("unmounted {p}"));
        }
    }
    // the root stays, read-only, so that nothing is half written
    // SAFETY: remount of "/" with the read-only flag, all pointers are valid for the call
    unsafe { libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), libc::MS_REMOUNT | libc::MS_RDONLY, std::ptr::null()) };
}
