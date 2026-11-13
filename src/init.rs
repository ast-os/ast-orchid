//! PID 1: bring the system up, keep the services running, answer `orchid`, take it all down.

use crate::console;
use crate::service::{self, Kind, Output, Restart, Service};
use crate::sys::{self, Halt, SignalFd};
use std::io::{BufRead, BufReader, Write};
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

#[derive(Clone, Copy, PartialEq)]
enum State {
    Idle,
    Up,
    Done,
    Failed,
}

struct Unit {
    svc: Service,
    pid: Option<i32>,
    state: State,
    since: Option<Instant>,
    exit: Option<i32>,
    restarts: Vec<Instant>,
    total_restarts: u32,
    restart_at: Option<Instant>,
    stopping: bool,
}

impl Unit {
    fn new(svc: Service) -> Unit {
        Unit { svc, pid: None, state: State::Idle, since: None, exit: None, restarts: vec![], total_restarts: 0, restart_at: None, stopping: false }
    }
}

struct Supervisor {
    units: Vec<Unit>,
    halt: Option<Halt>,
}

pub fn run() -> ! {
    console::banner();
    console::title(&format!("orchid {}", env!("CARGO_PKG_VERSION")));
    let signals = SignalFd::new();
    mount_basics();
    clean_tmp();
    hostname();
    let (services, mut problems) = service::load(Path::new(SERVICES));
    let (ordered, more) = service::order(services);
    problems.extend(more);
    for p in problems {
        console::warn(&p);
    }
    let mut sup = Supervisor { units: ordered.into_iter().map(Unit::new).collect(), halt: None };
    let listener = bind();
    sup.boot();
    let how = sup.serve(&signals, listener.as_ref());
    sup.shutdown(how)
}

fn mount_basics() {
    const FLAGS: u64 = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    let table: [(&str, &str, &str, u64, &str); 7] = [
        ("proc", "/proc", "proc", FLAGS, ""),
        ("sysfs", "/sys", "sysfs", FLAGS, ""),
        ("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID, "mode=0755"),
        ("devpts", "/dev/pts", "devpts", libc::MS_NOSUID | libc::MS_NOEXEC, "gid=5,mode=0620"),
        ("tmpfs", "/dev/shm", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, "mode=1777"),
        ("tmpfs", "/run", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, "mode=0755"),
        ("tmpfs", "/tmp", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, "mode=1777"),
    ];
    let mut done = Vec::new();
    for (source, target, fstype, flags, data) in table {
        if sys::is_mounted(target) {
            continue;
        }
        if sys::mount(source, target, fstype, flags, data) {
            if !target[1..].contains('/') && target != "/run" {
                done.push(target);
            }
        } else {
            console::warn(&format!("unable to mount {target}"));
        }
    }
    done.sort_by_key(|t| ["/sys", "/dev", "/proc", "/tmp"].iter().position(|x| x == t).unwrap_or(9));
    if !done.is_empty() {
        console::ok(&format!("mounting {}", done.join(" ")));
    }
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

fn spawn(svc: &Service) -> Result<i32, String> {
    let mut c = Command::new(&svc.exec[0]);
    c.args(&svc.exec[1..]).stdin(Stdio::null());
    for (k, v) in &svc.env {
        c.env(k, v);
    }
    if let Some(w) = &svc.workdir {
        c.current_dir(w);
    }
    let (uid, gid) = sys::ids(svc.user.as_deref(), svc.group.as_deref())?;
    if let Some(g) = gid {
        c.gid(g);
    }
    if let Some(u) = uid {
        c.uid(u);
    }
    let log = (svc.output == Output::File)
        .then(|| {
            let _ = std::fs::create_dir_all(LOGS);
            std::fs::OpenOptions::new().create(true).append(true).open(format!("{LOGS}/{}.log", svc.name)).ok()
        })
        .flatten();
    // without a log (the console was asked for, or the file cannot be opened yet) it prints to the console
    if let Some(f) = log {
        let g = f.try_clone().map_err(|e| e.to_string())?;
        c.stdout(f).stderr(g);
    }
    // SAFETY: setsid and sigprocmask are async-signal-safe and run between fork and exec. The
    // signals orchid blocks for itself must not stay blocked in what it starts.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            Ok(())
        });
    }
    let child = c.spawn().map_err(|e| format!("cannot run {}: {e}", svc.exec[0]))?;
    let pid = child.id() as i32;
    std::mem::forget(child); // reaped by the supervisor, not by Child
    Ok(pid)
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
            if enabled.as_ref().is_none_or(|e| e.contains(&self.units[i].svc.name))
                && let Err(e) = self.start_unit(i)
            {
                console::warn(&format!("{} {e}", self.units[i].svc.name));
            }
        }
    }

    /// Start a service after the ones it needs.
    fn start_unit(&mut self, i: usize) -> Result<(), String> {
        if matches!(self.units[i].state, State::Up | State::Done) {
            return Ok(());
        }
        let name = self.units[i].svc.name.clone();
        for n in self.units[i].svc.needs.clone() {
            let j = self.index(&n).ok_or_else(|| format!("needs {n}, which does not exist"))?;
            self.start_unit(j).map_err(|_| format!("skipped, {n} is not running"))?;
            if !matches!(self.units[j].state, State::Up | State::Done) {
                return Err(format!("skipped, {n} is not running"));
            }
        }
        let pid = match spawn(&self.units[i].svc) {
            Ok(p) => p,
            Err(e) => {
                self.units[i].state = State::Failed;
                return Err(format!("failed: {e}"));
            }
        };
        self.units[i].stopping = false;
        self.units[i].since = Some(Instant::now());
        if self.units[i].svc.kind == Kind::Daemon {
            self.units[i].pid = Some(pid);
            self.units[i].state = State::Up;
            console::ok(&format!("started {name}"));
            return Ok(());
        }
        // a oneshot finishes before anything that needs it starts
        let end = Instant::now() + ONESHOT_LIMIT;
        let code = loop {
            match sys::reap() {
                Some((p, code)) if p == pid => break code,
                Some((p, code)) => self.exited(p, code),
                None if Instant::now() > end => {
                    sys::kill(pid, libc::SIGKILL);
                    break 137;
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        self.units[i].exit = Some(code);
        if code == 0 {
            self.units[i].state = State::Done;
            console::ok(&format!("ran {name}"));
            Ok(())
        } else {
            self.units[i].state = State::Failed;
            Err(format!("failed (exit {code})"))
        }
    }

    /// Stop a service, and first the ones that need it.
    fn stop_unit(&mut self, i: usize) -> Result<(), String> {
        let name = self.units[i].svc.name.clone();
        for j in 0..self.units.len() {
            if j != i && self.units[j].state == State::Up && self.units[j].svc.needs.contains(&name) {
                self.stop_unit(j)?;
            }
        }
        let Some(pid) = self.units[i].pid else {
            self.units[i].state = State::Idle;
            return Ok(());
        };
        self.units[i].stopping = true;
        self.units[i].restart_at = None;
        sys::kill(pid, libc::SIGTERM);
        let end = Instant::now() + Duration::from_secs(self.units[i].svc.timeout);
        while self.units[i].pid.is_some() && Instant::now() < end {
            match sys::reap() {
                Some((p, code)) => self.exited(p, code),
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        if self.units[i].pid.is_some() {
            sys::kill(pid, libc::SIGKILL);
            std::thread::sleep(Duration::from_millis(100));
            while let Some((p, code)) = sys::reap() {
                self.exited(p, code);
            }
            self.units[i].pid = None;
            self.units[i].state = State::Idle;
            console::gone(&format!("killed {name}"));
        }
        Ok(())
    }

    /// A child ended: a service is restarted if its policy says so; anything else was an orphan.
    fn exited(&mut self, pid: i32, code: i32) {
        let Some(i) = self.units.iter().position(|u| u.pid == Some(pid)) else { return };
        let u = &mut self.units[i];
        u.pid = None;
        u.exit = Some(code);
        if u.stopping {
            u.stopping = false;
            u.state = State::Idle;
            console::gone(&format!("stopped {}", u.svc.name));
            return;
        }
        u.state = State::Failed;
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

    fn restart_due(&mut self) {
        let now = Instant::now();
        for i in 0..self.units.len() {
            if self.units[i].restart_at.is_some_and(|t| t <= now) {
                self.units[i].restart_at = None;
                self.units[i].state = State::Idle;
                if let Err(e) = self.start_unit(i) {
                    console::warn(&format!("{} {e}", self.units[i].svc.name));
                }
            }
        }
    }

    /// Serve until something asks to stop: TERM or INT reboot, USR1 halts, USR2 powers off, or
    /// a command over the socket.
    fn serve(&mut self, signals: &SignalFd, listener: Option<&UnixListener>) -> Halt {
        let mut fds = vec![signals.fd()];
        fds.extend(listener.map(|l| l.as_raw_fd()));
        loop {
            let ready = sys::poll(&fds, 500);
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
            self.restart_due();
            if let (Some(l), Some(true)) = (listener, ready.get(1)) {
                while let Ok((stream, _)) = l.accept() {
                    self.client(stream);
                    if let Some(h) = self.halt {
                        return h;
                    }
                }
            }
        }
    }

    fn client(&mut self, mut stream: std::os::unix::net::UnixStream) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let uid = sys::peer_uid(stream.as_raw_fd()).unwrap_or(u32::MAX);
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            return;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let reply = match words.split_first() {
            None => Err("empty command".to_string()),
            Some((cmd, args)) => self.handle(cmd, args, uid),
        };
        let _ = match reply {
            Ok(text) => write!(stream, "ok\n{text}"),
            Err(e) => writeln!(stream, "error {e}"),
        };
    }

    fn handle(&mut self, cmd: &str, args: &[&str], uid: u32) -> Result<String, String> {
        if !matches!(cmd, "list" | "status" | "version") && uid != 0 {
            return Err("only root may do that".into());
        }
        let named = |sup: &Supervisor, args: &[&str]| -> Result<Vec<usize>, String> {
            if args.is_empty() {
                return Err(format!("{cmd} needs a service name"));
            }
            args.iter().map(|n| sup.index(n).ok_or_else(|| format!("no service {n:?}"))).collect()
        };
        match cmd {
            "version" => Ok(format!("orchid  {}\n", env!("CARGO_PKG_VERSION"))),
            "list" => Ok(self.list()),
            "status" => {
                let i = *named(self, args)?.first().unwrap_or(&0);
                Ok(self.status(i))
            }
            "start" => {
                for i in named(self, args)? {
                    self.start_unit(i).map_err(|e| format!("{}: {e}", self.units[i].svc.name))?;
                }
                Ok(String::new())
            }
            "stop" => {
                for i in named(self, args)? {
                    self.stop_unit(i)?;
                }
                Ok(String::new())
            }
            "restart" => {
                for i in named(self, args)? {
                    self.stop_unit(i)?;
                    self.units[i].state = State::Idle;
                    self.start_unit(i).map_err(|e| format!("{}: {e}", self.units[i].svc.name))?;
                }
                Ok(String::new())
            }
            "enable" | "disable" => {
                let idx = named(self, args)?;
                let names: Vec<String> = idx.iter().map(|&i| self.units[i].svc.name.clone()).collect();
                set_enabled(&names, cmd == "enable")?;
                Ok(String::new())
            }
            "reload" => Ok(self.reload()),
            "reboot" => self.stop_all(Halt::Reboot),
            "poweroff" => self.stop_all(Halt::Poweroff),
            "halt" => self.stop_all(Halt::Stop),
            other => Err(format!("unknown command {other:?}")),
        }
    }

    fn stop_all(&mut self, how: Halt) -> Result<String, String> {
        self.halt = Some(how);
        Ok(String::new())
    }

    fn list(&self) -> String {
        let enabled = service::enabled(Path::new(ENABLED));
        let mut out = String::new();
        for u in &self.units {
            let state = match u.state {
                State::Idle => "stopped",
                State::Up => "up",
                State::Done => "done",
                State::Failed => "failed",
            };
            let boot = if enabled.as_ref().is_none_or(|e| e.contains(&u.svc.name)) { "on" } else { "off" };
            let detail = match (u.state, u.pid, u.exit) {
                (State::Up, Some(p), _) => format!("pid {p}  {}", age(u.since)),
                (State::Failed, _, Some(c)) => format!("exit {c}"),
                _ => String::new(),
            };
            out += &[u.svc.name.as_str(), state, boot, detail.as_str()].iter().filter(|c| !c.is_empty()).copied().collect::<Vec<_>>().join("  ");
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
        let state = match u.state {
            State::Idle => "stopped",
            State::Up => "up",
            State::Done => "done",
            State::Failed => "failed",
        };
        rows.push(("state".into(), state.into()));
        if let Some(p) = u.pid {
            rows.push(("pid".into(), p.to_string()));
            rows.push(("up for".into(), age(u.since)));
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

    /// Read the service files again: new ones appear (and start, if enabled), stopped ones are
    /// redefined, vanished stopped ones go; running ones keep running as they are.
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
                    if u.state != State::Up {
                        changed += 1;
                        u.svc = svc;
                    }
                    units.push(u);
                }
                None => {
                    added += 1;
                    units.push(Unit::new(svc));
                }
            }
        }
        // a service that is running and no longer has a file stays until it is stopped
        for u in self.units.drain(..) {
            if u.state == State::Up && !units.iter().any(|x| x.svc.name == u.svc.name) {
                units.push(u);
            }
        }
        self.units = units;
        for i in 0..self.units.len() {
            if self.units[i].state == State::Idle && enabled.as_ref().is_none_or(|e| e.contains(&self.units[i].svc.name)) && self.units[i].pid.is_none() && self.units[i].exit.is_none() {
                let _ = self.start_unit(i);
            }
        }
        let mut out = format!("{added} added  {changed} updated\n");
        for p in problems {
            out += &format!("{p}\n");
        }
        out
    }

    fn shutdown(&mut self, how: Halt) -> ! {
        console::title("shutting down");
        for i in (0..self.units.len()).rev() {
            if self.units[i].state == State::Up {
                let _ = self.stop_unit(i);
            }
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
