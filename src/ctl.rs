//! `orchid` run by anyone but PID 1: it sends a command to PID 1 and prints the answer.

use crate::init::{LOGS, SOCKET};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

pub fn send(words: &[&str]) -> i32 {
    let mut s = match UnixStream::connect(SOCKET) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("orchid: cannot reach PID 1 ({SOCKET}): {e}");
            return 1;
        }
    };
    let _ = s.write_all(format!("{}\n", words.join(" ")).as_bytes());
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    match out.split_once('\n') {
        Some(("ok", rest)) => {
            print!("{rest}");
            0
        }
        Some((head, _)) if head.starts_with("error ") => {
            eprintln!("orchid: {}", &head[6..]);
            1
        }
        // the system went down before it answered
        None if words.first().is_some_and(|w| matches!(*w, "reboot" | "poweroff" | "halt")) => 0,
        _ => {
            eprintln!("orchid: no answer");
            1
        }
    }
}

/// The last lines of a service's log.
pub fn log(name: &str, lines: usize) -> i32 {
    let path = format!("{LOGS}/{name}.log");
    match std::fs::read_to_string(&path) {
        Ok(t) => {
            let all: Vec<&str> = t.lines().collect();
            for l in &all[all.len().saturating_sub(lines)..] {
                println!("{l}");
            }
            0
        }
        Err(e) => {
            eprintln!("orchid: no log of {name}: {e}");
            1
        }
    }
}
