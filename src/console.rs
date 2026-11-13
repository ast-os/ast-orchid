//! The boot screen: the banner, then one line per step, each with a marker.
//! `[#]` a heading, `[+]` done, `[!]` something went wrong, `[-]` taken down.

use std::io::Write;

pub const BANNER: &str = include_str!("banner.txt");

fn line(marker: char, text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "[{marker}] {text}");
    let _ = out.flush();
}

pub fn banner() {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "{BANNER}\n");
    let _ = out.flush();
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
