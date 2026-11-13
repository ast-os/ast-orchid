//! PID 1: bring the system up, keep the services running, answer `orchid`, take it all down.
//!
//! One loop waits for signals (SIGCHLD above all) and for the control socket. Starting and
//! stopping never wait inside it: every service has a state, a request only changes what is
//! wanted, and `advance` moves each service as far as its dependencies allow. A control command
//! that waits for a result is kept in `waiters` and answered when the services it names settle.

use crate::console;
use crate::service::{self, Kind, Output, Restart, Service};
use crate::sys::{self, Halt, SignalFd};
use std::io::{BufRead, BufReader, Write};
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
}

impl Unit {
    fn new(svc: Service) -> Unit {
        Unit { svc, pid: None, state: State::Idle, want: false, since: None, exit: None, why: None, deadline: None, restarts: vec![], total_restarts: 0, restart_at: None }
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
    let mut sup = Supervisor { units: ordered.into_iter().map(Unit::new).collect(), halt: None, waiters: vec![] };
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
            if enabled.as_ref().is_none_or(|e| e.contains(&self.units[i].svc.name)) {
                self.want_up(i);
            }
        }
        self.advance();
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
        let pid = match spawn(&self.units[i].svc) {
            Ok(p) => p,
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

    fn terminate(&mut self, i: usize) {
        let u = &mut self.units[i];
        match u.pid {
            Some(pid) => {
                sys::kill(pid, libc::SIGTERM);
                u.state = State::Stopping;
                u.deadline = Some(Instant::now() + Duration::from_secs(u.svc.timeout));
            }
            None => u.state = State::Idle,
        }
    }

    /// A child ended: a service is restarted if its policy says so; anything else was an orphan.
    fn exited(&mut self, pid: i32, code: i32) {
        let Some(i) = self.units.iter().position(|u| u.pid == Some(pid)) else { return };
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

    /// What time has to say: restarts that are due, services that do not stop, oneshots that do not end.
    fn timers(&mut self) {
        let now = Instant::now();
        for u in &mut self.units {
            if u.restart_at.is_some_and(|t| t <= now) {
                u.restart_at = None;
                u.state = State::Idle;
            }
            if u.deadline.is_some_and(|t| t <= now) {
                u.deadline = None;
                if let Some(pid) = u.pid {
                    sys::kill(pid, libc::SIGKILL);
                    console::gone(&format!("killed {}", u.svc.name));
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
        let mut fds = vec![signals.fd()];
        fds.extend(listener.map(|l| l.as_raw_fd()));
        loop {
            let ready = sys::poll(&fds, self.next_timer());
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
            if u.state == State::Idle && !u.want && u.exit.is_none() && enabled.as_ref().is_none_or(|e| e.contains(&u.svc.name)) {
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
