//! The few system calls an init needs: mount, unmount, reboot, hostname, signals.

use std::ffi::CString;

fn c(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

/// Is something mounted at `dir`?
pub fn is_mounted(dir: &str) -> bool {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|m| m.lines().any(|l| l.split(' ').nth(4) == Some(dir)))
        .unwrap_or(false)
}

pub fn mount(source: &str, target: &str, fstype: &str, flags: u64, data: &str) -> bool {
    let _ = std::fs::create_dir_all(target);
    // SAFETY: all pointers come from live CStrings
    unsafe { libc::mount(c(source).as_ptr(), c(target).as_ptr(), c(fstype).as_ptr(), flags, c(data).as_ptr().cast()) == 0 }
}

pub fn umount(target: &str) -> bool {
    // SAFETY: the pointer comes from a live CString
    unsafe { libc::umount2(c(target).as_ptr(), 0) == 0 }
}

pub fn set_hostname(name: &str) -> bool {
    // SAFETY: pointer and length describe `name`
    unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) == 0 }
}

pub fn sync() {
    // SAFETY: no arguments
    unsafe { libc::sync() }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Halt {
    Reboot,
    Poweroff,
    Stop,
}

pub fn halt(how: Halt) -> ! {
    let cmd = match how {
        Halt::Reboot => libc::RB_AUTOBOOT,
        Halt::Poweroff => libc::RB_POWER_OFF,
        Halt::Stop => libc::RB_HALT_SYSTEM,
    };
    // SAFETY: the call does not return on success
    unsafe { libc::reboot(cmd) };
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

pub fn kill(pid: i32, sig: i32) {
    // SAFETY: plain signal delivery
    unsafe { libc::kill(pid, sig) };
}

/// Signals orchid answers.
const SIGNALS: [i32; 5] = [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT, libc::SIGUSR1, libc::SIGUSR2];

/// The signals above, blocked and delivered through a file descriptor so that they can be waited
/// for together with the control socket.
pub struct SignalFd(i32);

impl SignalFd {
    pub fn new() -> SignalFd {
        // SAFETY: the set is initialised by sigemptyset before use
        let fd = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for s in SIGNALS {
                libc::sigaddset(&mut set, s);
            }
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK)
        };
        SignalFd(fd)
    }

    pub fn fd(&self) -> i32 {
        self.0
    }

    /// The signals that arrived.
    pub fn read(&self) -> Vec<i32> {
        let size = std::mem::size_of::<libc::signalfd_siginfo>();
        let mut out = Vec::new();
        loop {
            // SAFETY: `info` is a plain struct of `size` bytes
            let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
            let n = unsafe { libc::read(self.0, (&mut info as *mut libc::signalfd_siginfo).cast(), size) };
            if n != size as isize {
                return out;
            }
            out.push(info.ssi_signo as i32);
        }
    }
}

/// Wait up to `ms` for any of the descriptors to be readable; which ones are.
pub fn poll(fds: &[i32], ms: i32) -> Vec<bool> {
    let mut v: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    // SAFETY: `v` is a valid array of pollfd
    unsafe { libc::poll(v.as_mut_ptr(), v.len() as libc::nfds_t, ms) };
    v.iter().map(|p| p.revents & libc::POLLIN != 0).collect()
}

/// The user id of the process at the other end of a Unix socket.
pub fn peer_uid(fd: i32) -> Option<u32> {
    // SAFETY: `cred` is a plain struct and `len` its size
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) };
    (r == 0).then_some(cred.uid)
}

/// The numeric ids of a user and a group named in a service file.
pub fn ids(user: Option<&str>, group: Option<&str>) -> Result<(Option<u32>, Option<u32>), String> {
    let find = |file: &str, name: &str, field: usize| -> Result<u32, String> {
        let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
        text.lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                (f.first() == Some(&name)).then(|| f.get(field).and_then(|v| v.parse().ok())).flatten()
            })
            .next()
            .ok_or_else(|| format!("no such {} {name:?}", if file.ends_with("passwd") { "user" } else { "group" }))
    };
    let uid = user.map(|u| u.parse().or_else(|_| find("/etc/passwd", u, 2))).transpose()?;
    let gid = match (group, user) {
        (Some(g), _) => Some(g.parse().or_else(|_| find("/etc/group", g, 2))?),
        (None, Some(u)) => Some(find("/etc/passwd", u, 3).or_else(|e| u.parse().map_err(|_| e))?),
        _ => None,
    };
    Ok((uid, gid))
}

/// A finished child, if any: `(pid, exit code or 128 + signal)`.
pub fn reap() -> Option<(i32, i32)> {
    let mut status = 0;
    // SAFETY: `status` is a valid out pointer
    let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
    if pid <= 0 {
        return None;
    }
    let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) };
    Some((pid, code))
}

/// The control groups of the services: one directory per service in the unified hierarchy, so
/// that everything a service starts can be found, signalled and killed with it.
pub mod cgroup {
    use std::path::PathBuf;

    const ROOT: &str = "/sys/fs/cgroup/orchid";

    /// Make the directory for the services; false when there is no cgroup v2.
    pub fn init() -> bool {
        let _ = std::fs::create_dir_all(ROOT);
        std::path::Path::new(ROOT).join("cgroup.procs").exists()
    }

    pub fn dir(name: &str) -> PathBuf {
        PathBuf::from(ROOT).join(name)
    }

    pub fn create(name: &str) -> Option<PathBuf> {
        let d = dir(name);
        std::fs::create_dir_all(&d).ok()?;
        Some(d.join("cgroup.procs"))
    }

    pub fn procs(name: &str) -> Vec<i32> {
        std::fs::read_to_string(dir(name).join("cgroup.procs")).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).collect()
    }

    pub fn populated(name: &str) -> bool {
        std::fs::read_to_string(dir(name).join("cgroup.events")).is_ok_and(|t| t.lines().any(|l| l == "populated 1"))
    }

    /// Signal every process of the service.
    pub fn signal(name: &str, sig: i32) {
        for p in procs(name) {
            super::kill(p, sig);
        }
    }

    /// Kill every process of the service at once.
    pub fn kill(name: &str) {
        if std::fs::write(dir(name).join("cgroup.kill"), "1").is_err() {
            signal(name, libc::SIGKILL);
        }
    }

    /// Kill what is left in the service's group, then remove the group.
    pub fn clear(name: &str) {
        kill(name);
        let _ = std::fs::remove_dir(dir(name));
    }
}
