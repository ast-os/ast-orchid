# orchid

The init system of asterOS. A single static binary, written in Rust.

As PID 1 it prints the boot screen, mounts what is missing (`/proc /sys /dev /run /tmp`),
cleans `/tmp`, sets the host name from `/etc/hostname`, starts the services, keeps them running,
and takes the system down cleanly.

```
cargo build --release --target x86_64-unknown-linux-musl     # one static file
```

## Services

One TOML file per service in `/etc/orchid/services`:

```toml
[service]
description = "secure shell"
exec = ["/usr/sbin/sshd", "-D"]
needs = ["network"]            # started after these; a missing one is reported
type = "daemon"                # daemon (default) or oneshot: finishes before others start
restart = "on-failure"         # no (default), on-failure or always; five restarts in 30 s, then it gives up
```

## Commands

```
orchid reboot | poweroff | halt    ask PID 1 to take the system down
orchid check [DIR]                 check the service files and show the start order
```

PID 1 also answers signals: TERM and INT reboot, USR1 halts, USR2 powers off.

## Test

`tests/boot.sh` boots it as PID 1 in qemu with a few services.
