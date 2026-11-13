//! PID 1: bring the system up, keep the services running, take it down.

use crate::console;
use crate::service::{self, Kind, Restart, Service};
use crate::sys::{self, Halt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SERVICES: &str = "/etc/orchid/services";
const RESTARTS: usize = 5;
const WINDOW: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq)]
enum State {
    New,
    Up,
    Done,
    Failed,
}

struct Unit {
    svc: Service,
    pid: Option<i32>,
    state: State,
    restarts: Vec<Instant>,
    restart_at: Option<Instant>,
}

struct Supervisor {
    units: Vec<Unit>,
}

pub fn run() -> ! {
    console::banner();
    console::title(&format!("orchid {}", env!("CARGO_PKG_VERSION")));
    sys::block_signals();
    mount_basics();
    clean_tmp();
    hostname();
    let (services, mut problems) = service::load(Path::new(SERVICES));
    let (ordered, more) = service::order(services);
    problems.extend(more);
    for p in problems {
        console::warn(&p);
    }
    let mut sup = Supervisor { units: ordered.into_iter().map(|svc| Unit { svc, pid: None, state: State::New, restarts: vec![], restart_at: None }).collect() };
    sup.start_all();
    let how = sup.run();
    sup.shutdown(how)
}

fn mount_basics() {
    const FLAGS: u64 = (libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC) as u64;
    let table: [(&str, &str, &str, u64, &str); 7] = [
        ("proc", "/proc", "proc", FLAGS, ""),
        ("sysfs", "/sys", "sysfs", FLAGS, ""),
        ("devtmpfs", "/dev", "devtmpfs", (libc::MS_NOSUID) as u64, "mode=0755"),
        ("devpts", "/dev/pts", "devpts", (libc::MS_NOSUID | libc::MS_NOEXEC) as u64, "gid=5,mode=0620"),
        ("tmpfs", "/dev/shm", "tmpfs", (libc::MS_NOSUID | libc::MS_NODEV) as u64, "mode=1777"),
        ("tmpfs", "/run", "tmpfs", (libc::MS_NOSUID | libc::MS_NODEV) as u64, "mode=0755"),
        ("tmpfs", "/tmp", "tmpfs", (libc::MS_NOSUID | libc::MS_NODEV) as u64, "mode=1777"),
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

fn spawn(svc: &Service) -> Result<i32, String> {
    let mut c = Command::new(&svc.exec[0]);
    c.args(&svc.exec[1..]).stdin(Stdio::null());
    // SAFETY: setsid is async-signal-safe and runs between fork and exec
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = c.spawn().map_err(|e| format!("cannot run {}: {e}", svc.exec[0]))?;
    let pid = child.id() as i32;
    std::mem::forget(child); // reaped by the supervisor, not by Child
    Ok(pid)
}

impl Supervisor {
    fn index_of(&self, name: &str) -> Option<usize> {
        self.units.iter().position(|u| u.svc.name == name)
    }

    fn start_all(&mut self) {
        for i in 0..self.units.len() {
            self.start(i);
        }
    }

    fn start(&mut self, i: usize) {
        let name = self.units[i].svc.name.clone();
        let blocked = self.units[i].svc.needs.iter().find(|n| self.index_of(n).is_none_or(|j| !matches!(self.units[j].state, State::Up | State::Done))).cloned();
        if let Some(need) = blocked {
            console::warn(&format!("{name} skipped, {need} is not running"));
            self.units[i].state = State::Failed;
            return;
        }
        match spawn(&self.units[i].svc) {
            Err(e) => {
                console::warn(&format!("{name} failed: {e}"));
                self.units[i].state = State::Failed;
            }
            Ok(pid) if self.units[i].svc.kind == Kind::Daemon => {
                self.units[i].pid = Some(pid);
                self.units[i].state = State::Up;
                console::ok(&format!("started {name}"));
            }
            Ok(pid) => {
                // a oneshot finishes before anything that needs it starts
                let code = loop {
                    match sys::reap() {
                        Some((p, code)) if p == pid => break code,
                        Some((p, code)) => self.exited(p, code),
                        None => {
                            sys::wait_signal(100);
                        }
                    }
                };
                if code == 0 {
                    self.units[i].state = State::Done;
                    console::ok(&format!("ran {name}"));
                } else {
                    self.units[i].state = State::Failed;
                    console::warn(&format!("{name} failed (exit {code})"));
                }
            }
        }
    }

    /// A child ended: a service is restarted if its policy says so, anything else was an orphan.
    fn exited(&mut self, pid: i32, code: i32) {
        let Some(i) = self.units.iter().position(|u| u.pid == Some(pid)) else { return };
        let u = &mut self.units[i];
        u.pid = None;
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
        u.restart_at = Some(now + Duration::from_secs(1));
        console::warn(&format!("{} stopped (exit {code}), restarting", u.svc.name));
    }

    fn restart_due(&mut self) {
        let now = Instant::now();
        for i in 0..self.units.len() {
            if self.units[i].restart_at.is_some_and(|t| t <= now) {
                self.units[i].restart_at = None;
                self.units[i].state = State::New;
                match spawn(&self.units[i].svc) {
                    Ok(pid) => {
                        self.units[i].pid = Some(pid);
                        self.units[i].state = State::Up;
                        console::ok(&format!("started {}", self.units[i].svc.name));
                    }
                    Err(e) => console::warn(&format!("{} failed: {e}", self.units[i].svc.name)),
                }
            }
        }
    }

    /// Serve until a signal asks to stop: TERM or INT reboot, USR1 halts, USR2 powers off.
    fn run(&mut self) -> Halt {
        loop {
            match sys::wait_signal(500) {
                Some(libc::SIGTERM | libc::SIGINT) => return Halt::Reboot,
                Some(libc::SIGUSR1) => return Halt::Stop,
                Some(libc::SIGUSR2) => return Halt::Poweroff,
                _ => {}
            }
            while let Some((pid, code)) = sys::reap() {
                self.exited(pid, code);
            }
            self.restart_due();
        }
    }

    fn shutdown(&mut self, how: Halt) -> ! {
        console::title("shutting down");
        for i in (0..self.units.len()).rev() {
            let Some(pid) = self.units[i].pid else { continue };
            sys::kill(pid, libc::SIGTERM);
            let end = Instant::now() + Duration::from_secs(5);
            while Instant::now() < end && self.units[i].pid.is_some() {
                match sys::reap() {
                    Some((p, _)) if p == pid => self.units[i].pid = None,
                    Some(_) => {}
                    None => {
                        sys::wait_signal(100);
                    }
                }
            }
            if self.units[i].pid.is_some() {
                sys::kill(pid, libc::SIGKILL);
            }
            console::gone(&format!("stopped {}", self.units[i].svc.name));
        }
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
    unsafe { libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), (libc::MS_REMOUNT | libc::MS_RDONLY) as u64, std::ptr::null()) };
}
