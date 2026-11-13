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

/// Signals orchid answers; they are blocked so that they can be waited for.
pub const SIGNALS: [i32; 5] = [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT, libc::SIGUSR1, libc::SIGUSR2];

fn sigset() -> libc::sigset_t {
    // SAFETY: the set is initialised by sigemptyset before use
    unsafe {
        let mut set = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in SIGNALS {
            libc::sigaddset(&mut set, s);
        }
        set
    }
}

pub fn block_signals() {
    let set = sigset();
    // SAFETY: `set` is valid
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
}

/// Wait up to `ms` for one of the signals; `None` on timeout.
pub fn wait_signal(ms: i64) -> Option<i32> {
    let set = sigset();
    let ts = libc::timespec { tv_sec: ms / 1000, tv_nsec: (ms % 1000) * 1_000_000 };
    // SAFETY: `set` and `ts` are valid
    let s = unsafe { libc::sigtimedwait(&set, std::ptr::null_mut(), &ts) };
    (s > 0).then_some(s)
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
