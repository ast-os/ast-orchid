//! orchid: the init system of asterOS. As PID 1 it boots the system; run by anyone else it
//! tells PID 1 what to do, or checks the service files.

mod console;
mod ctl;
mod init;
mod service;
mod sys;

use std::path::Path;

const HELP: &str = "orchid

  list  services
  status NAME  one service
  start | stop | restart NAME...
  kill NAME [SIGNAL]
  enable | disable NAME...
  reload  reread the service files
  log NAME [LINES]
  reboot | poweroff | halt
  check [DIR]  check the service files
  version";

fn main() {
    if std::process::id() == 1 {
        init::run();
    }
    let args: Vec<String> = std::env::args().collect();
    // `reboot`, `poweroff` and `halt` may be links to orchid
    let me = Path::new(&args[0]).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let words: Vec<&str> = if matches!(me.as_str(), "reboot" | "poweroff" | "halt") {
        vec![me.as_str()]
    } else {
        args.iter().skip(1).map(String::as_str).collect()
    };
    let code = match words.as_slice() {
        ["list" | "ls"] => ctl::send(&["list"]),
        ["status", _] | ["kill", _] | ["kill", _, _] | ["start" | "stop" | "restart" | "enable" | "disable", _, ..] | ["reload"] | ["reboot"] | ["poweroff"] | ["halt"] => ctl::send(&words),
        ["log", name] => ctl::log(name, 20),
        ["log", name, n] => ctl::log(name, n.parse().unwrap_or(20)),
        ["check", rest @ ..] => check(rest.first().copied().unwrap_or(init::SERVICES)),
        ["version"] | ["-V"] | ["--version"] => ctl::send(&["version"]).max(0),
        [] | ["help"] | ["-h"] | ["--help"] => {
            println!("{HELP}");
            0
        }
        _ => {
            eprintln!("orchid: wrong arguments; see orchid help");
            2
        }
    };
    std::process::exit(code);
}

fn check(dir: &str) -> i32 {
    let (services, mut problems) = service::load(Path::new(dir));
    let (ordered, more) = service::order(services);
    problems.extend(more);
    for p in &problems {
        eprintln!("{p}");
    }
    for s in &ordered {
        println!("{}", [s.name.as_str(), s.description.as_str()].iter().filter(|c| !c.is_empty()).copied().collect::<Vec<_>>().join("  "));
    }
    i32::from(!problems.is_empty())
}
