# orchid

The init system of asterOS. One static binary, written in Rust.

As PID 1 it mounts what the kernel left unmounted, starts the services, keeps them running,
answers the `orchid` command and takes the system down cleanly. Run by anyone else, `orchid`
is the control tool.

```
cargo build --release --target x86_64-unknown-linux-musl     # one static file
```

## Commands

```
orchid list                      the services: state, whether they start at boot, pid and age
orchid status NAME               everything about one service
orchid start | stop | restart NAME...
orchid enable | disable NAME...  start at boot, or not
orchid kill NAME [SIGNAL]       signal every process of a service (default TERM)
orchid reload                    read the service files again
orchid log NAME [LINES]          the end of a service's log
orchid reboot | poweroff | halt  take the system down (also as links named reboot, poweroff, halt)
orchid check [DIR]               check the service files and show the start order
```

Anyone may `list` and `status`; changing anything needs root. PID 1 also answers signals:
TERM and INT reboot, USR1 halts, USR2 powers off.

Nothing waits inside PID 1: every service has a state, and a request only changes what is
wanted. Services whose dependencies are met start together; `orchid` answers while a oneshot is
still running. `start`, `stop` and `restart` reply when the services they name have settled
(up or done, or failed, with the reason). Starting a service starts what it needs first; stopping
one stops what needs it first. A service that cannot start is reported and the rest of the boot
goes on.

## Services

One TOML file per service in `/etc/orchid/services`, started at boot when linked from
`/etc/orchid/enabled` (without that directory every service starts; the first `enable` or
`disable` creates it).

```toml
[service]
description = "secure shell"
exec = ["/usr/sbin/sshd", "-D"]
type = "daemon"                # daemon (default, stays in the foreground), oneshot (what needs it waits until it has finished) or forking (starts a daemon that leaves: up while its control group has processes)
needs = ["network"]            # started first; if it fails, this one is skipped
wants = ["log"]                # started first if it exists, no more
after = ["clock"]              # only the order
restart = "on-failure"         # no (default), on-failure or always; five restarts in 30 s, then it gives up
user = "sshd"                  # a name or a number, with group = "..." (default: the user's own)
workdir = "/"
output = "file"                # file (default for daemons: /var/log/orchid/NAME.log), console (default for oneshots) or tty (the console is its terminal)
timeout = 5                    # seconds a stopping service gets before it is killed
stop = ["/usr/bin/sshd-save"]  # run when it is stopped, before it is signalled (a oneshot that is done: when it is stopped)
modes = ["normal"]             # the boot modes it belongs to: normal, rescue
env = { LANG = "C" }
```

## What the system does for a service

- **Control groups.** Every service runs in its own cgroup v2 (`/sys/fs/cgroup/orchid/NAME`). `stop`
  and `kill` reach everything the service started, and what a service leaves behind when it ends is
  killed with it. `status` shows how many processes it has.
- **Logs.** What a service prints goes through PID 1 to `/var/log/orchid/NAME.log`, every line with
  the time. A log that would pass 1 MiB is rotated (`.log.1` to `.log.5`).
- **Consoles.** The kernel can have several (`console=tty0 console=ttyS0`); orchid writes to every
  active one, so the screen and the serial line both show the boot.
- **Boot modes.** `rescue` (or `single`) on the kernel command line starts only the services that
  list `rescue` in `modes`. A service with `output = "tty"` gets the console as its terminal.

## Services of orchid itself

`fsck` (the file systems, the root while it is read-only), `rootfs` (made read-write), `mounts`,
`loopback`, `efivars`, `random-seed` (restored at boot, saved at shutdown), `hwclock` (set the time,
save it at shutdown), `sysctl`, `swap` and `rescue-shell`. The others come with the package of the
daemon they start: `udevd` with eudev, `agetty-*` with util-linux, `syslogd` with sysklogd, `dhcpcd`
with dhcpcd.

## Test

`tests/boot.sh` boots orchid as PID 1 in qemu with services that drive every command.
