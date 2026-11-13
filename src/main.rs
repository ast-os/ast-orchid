//! orchid: the init system of asterOS. As PID 1 it boots the system; run by anyone else it
//! tells PID 1 what to do, or checks the service files.

mod console;
mod init;
mod service;
mod sys;

use std::path::Path;

const HELP: &str = "orchid  the init system of asterOS

  reboot | poweroff | halt   ask PID 1 to take the system down
  check [DIR]                check the service files (default /etc/orchid/services)";

fn main() {
    if std::process::id() == 1 {
        init::run();
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["reboot"] => sys::kill(1, libc::SIGTERM),
        ["poweroff"] => sys::kill(1, libc::SIGUSR2),
        ["halt"] => sys::kill(1, libc::SIGUSR1),
        ["check", rest @ ..] => {
            let dir = rest.first().copied().unwrap_or("/etc/orchid/services");
            let (services, mut problems) = service::load(Path::new(dir));
            let (ordered, more) = service::order(services);
            problems.extend(more);
            for p in &problems {
                eprintln!("{p}");
            }
            for s in &ordered {
                println!("{}", [s.name.as_str(), s.description.as_str()].iter().filter(|c| !c.is_empty()).copied().collect::<Vec<_>>().join("  "));
            }
            std::process::exit(i32::from(!problems.is_empty()));
        }
        ["-V"] | ["--version"] => println!("orchid {}", env!("CARGO_PKG_VERSION")),
        _ => println!("{HELP}"),
    }
}
