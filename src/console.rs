//! The boot screen: the banner, then one line per step, each with a marker.
//! `[#]` a heading, `[+]` done, `[!]` something went wrong, `[-]` taken down.
//!
//! The kernel can have several consoles (`console=tty0 console=ttyS0`): `/dev/console` is only the
//! last of them. What orchid prints goes to every active one, so the screen and the serial line
//! both show the boot.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;

pub const BANNER: &str = include_str!("banner.txt");

static CONSOLES: Mutex<Vec<File>> = Mutex::new(Vec::new());

/// Open the active consoles (`/sys/class/tty/console/active`); once `/dev` is there. Without any,
/// standard output is used.
pub fn open() {
    let mut list = CONSOLES.lock().unwrap_or_else(|e| e.into_inner());
    if !list.is_empty() {
        return;
    }
    let active = std::fs::read_to_string("/sys/class/tty/console/active").unwrap_or_default();
    for name in active.split_whitespace() {
        if let Ok(f) = OpenOptions::new().write(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(format!("/dev/{name}")) {
            list.push(f);
        }
    }
}

/// Text to every console.
pub fn write(text: &str) {
    let list = CONSOLES.lock().unwrap_or_else(|e| e.into_inner());
    if list.is_empty() {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
        return;
    }
    for mut f in list.iter() {
        let _ = f.write_all(text.as_bytes());
    }
}

fn line(marker: char, text: &str) {
    write(&format!("[{marker}] {text}\n"));
}

/// Only the errors of the kernel on the screen (the console log level); its warnings and notices
/// are in `dmesg`.
pub fn quiet_kernel() {
    // SAFETY: SYSLOG_ACTION_CONSOLE_LEVEL takes the level as its length and no buffer
    unsafe { libc::syscall(libc::SYS_syslog, 8, std::ptr::null::<u8>(), 3) };
}

/// Empty the screens (and the scrollback) of what the kernel and the initramfs wrote.
pub fn clear() {
    write("\x1b[H\x1b[2J\x1b[3J");
}

pub fn banner() {
    write(&format!("{BANNER}\n"));
}

pub fn title(text: &str) {
    line('#', text);
}

pub fn ok(text: &str) {
    line('+', text);
}

pub fn warn(text: &str) {
    line('!', text);
}

pub fn gone(text: &str) {
    line('-', text);
}
