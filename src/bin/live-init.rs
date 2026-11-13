//! `/init` of the live initramfs: find the medium, mount the squashfs under a tmpfs overlay,
//! switch to it and run orchid.
//!
//! The medium is whatever block device holds an ISO 9660 filesystem with `/live/root.squashfs`.

use std::ffi::CString;
use std::time::{Duration, Instant};

const MEDIUM: &str = "/medium";
const SQUASH: &str = "/live/root.squashfs";
const WAIT: Duration = Duration::from_secs(60);

fn c(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

fn mount(src: &str, dst: &str, fs: &str, flags: u64, data: &str) -> bool {
    let _ = std::fs::create_dir_all(dst);
    // SAFETY: all pointers come from live CStrings
    unsafe { libc::mount(c(src).as_ptr(), c(dst).as_ptr(), c(fs).as_ptr(), flags, c(data).as_ptr().cast()) == 0 }
}

fn say(msg: &str) {
    println!("[#] {msg}");
}

fn die(msg: &str) -> ! {
    println!("[!] {msg}");
    // nothing to fall back on: stay alive so the message can be read
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Block devices that may hold the medium: whole disks and partitions.
fn candidates() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            ["sr", "sd", "vd", "nvme", "mmcblk", "xvd"].iter().any(|p| n.starts_with(p)) && !n.starts_with("loop")
        })
        .collect();
    v.sort();
    v.into_iter().map(|n| format!("/dev/{n}")).collect()
}

fn find_medium() -> Option<String> {
    let t0 = Instant::now();
    while t0.elapsed() < WAIT {
        for dev in candidates() {
            if mount(&dev, MEDIUM, "iso9660", libc::MS_RDONLY, "") {
                if std::path::Path::new(&format!("{MEDIUM}{SQUASH}")).exists() {
                    return Some(dev);
                }
                // SAFETY: the pointer comes from a live CString
                unsafe { libc::umount2(c(MEDIUM).as_ptr(), 0) };
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

/// Attach `file` to a free loop device and return its path.
fn loop_attach(file: &str) -> Option<String> {
    use std::os::fd::AsRawFd;
    const LOOP_CTL_GET_FREE: i32 = 0x4C82;
    const LOOP_SET_FD: i32 = 0x4C00;
    let ctl = std::fs::OpenOptions::new().read(true).write(true).open("/dev/loop-control").ok()?;
    // SAFETY: the ioctl takes no pointer
    let n = unsafe { libc::ioctl(ctl.as_raw_fd(), LOOP_CTL_GET_FREE) };
    if n < 0 {
        return None;
    }
    let path = format!("/dev/loop{n}");
    let t0 = Instant::now();
    while !std::path::Path::new(&path).exists() && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let dev = std::fs::OpenOptions::new().read(true).write(true).open(&path).ok()?;
    let f = std::fs::File::open(file).ok()?;
    // SAFETY: the argument is a file descriptor that stays open for the call
    (unsafe { libc::ioctl(dev.as_raw_fd(), LOOP_SET_FD, f.as_raw_fd()) } == 0).then_some(path)
}

fn main() {
    mount("devtmpfs", "/dev", "devtmpfs", 0, "");
    mount("proc", "/proc", "proc", 0, "");
    mount("sysfs", "/sys", "sysfs", 0, "");
    say("asterOS live");

    let Some(dev) = find_medium() else { die("no medium with /live/root.squashfs found") };
    say(&format!("medium  {dev}"));
    let Some(lp) = loop_attach(&format!("{MEDIUM}{SQUASH}")) else { die("cannot set up a loop device") };
    if !mount(&lp, "/ro", "squashfs", libc::MS_RDONLY, "") {
        die("cannot mount the squashfs");
    }
    if !mount("tmpfs", "/rw", "tmpfs", 0, "mode=755") {
        die("cannot mount a tmpfs");
    }
    let _ = std::fs::create_dir_all("/rw/up");
    let _ = std::fs::create_dir_all("/rw/work");
    if !mount("overlay", "/newroot", "overlay", 0, "lowerdir=/ro,upperdir=/rw/up,workdir=/rw/work") {
        die("cannot mount the overlay");
    }
    // the way out of the initramfs: keep the mounts the system needs under the new root
    for d in ["dev", "proc", "sys"] {
        let dst = format!("/newroot/{d}");
        let _ = std::fs::create_dir_all(&dst);
        // SAFETY: both pointers come from live CStrings
        unsafe { libc::mount(c(&format!("/{d}")).as_ptr(), c(&dst).as_ptr(), std::ptr::null(), libc::MS_MOVE, std::ptr::null()) };
    }
    let _ = std::fs::create_dir_all("/newroot/medium");
    // SAFETY: both pointers come from live CStrings
    unsafe { libc::mount(c(MEDIUM).as_ptr(), c("/newroot/medium").as_ptr(), std::ptr::null(), libc::MS_MOVE, std::ptr::null()) };

    // SAFETY: plain calls with live CStrings; exec replaces the process
    unsafe {
        if libc::chdir(c("/newroot").as_ptr()) != 0
            || libc::mount(c(".").as_ptr(), c("/").as_ptr(), std::ptr::null(), libc::MS_MOVE, std::ptr::null()) != 0
            || libc::chroot(c(".").as_ptr()) != 0
        {
            die("cannot switch to the new root");
        }
        libc::chdir(c("/").as_ptr());
        let init = c("/usr/sbin/init");
        let argv = [init.as_ptr(), std::ptr::null()];
        libc::execv(init.as_ptr(), argv.as_ptr());
    }
    die("cannot run /usr/sbin/init");
}
