# orchid

Init system of asterOS: one static binary in Rust. As PID 1 it starts services and keeps them
running; run by anyone else it is the control tool.

```
cargo build --release --target x86_64-unknown-linux-musl
```

## Commands

```
orchid list | status NAME
orchid start | stop | restart NAME...
orchid enable | disable NAME...
orchid kill NAME [SIGNAL]
orchid log NAME [LINES]
orchid reload | check [DIR]
orchid reboot | poweroff | halt
```

Anyone may `list` and `status`; changes need root.

## Services

One TOML file per service in `/etc/orchid/services`:

```toml
[service]
description = "secure shell"
exec = ["/usr/sbin/sshd", "-D"]
type = "daemon"            # daemon, oneshot or forking
needs = ["network"]        # started first; if it fails, this one is skipped
wants = ["log"]            # started first if it exists
after = ["clock"]          # order only
restart = "on-failure"     # no, on-failure or always
user = "sshd"
output = "file"            # file, console or tty
timeout = 5                # seconds before a stopping service is killed
stop = ["/usr/bin/sshd-save"]
modes = ["normal"]         # normal, rescue
```

Every service has its own cgroup, a log in `/var/log/orchid/NAME.log` (rotated), and output on
every active console. `rescue` on the kernel command line starts only rescue services.

Test: `tests/boot.sh` boots orchid as PID 1 in qemu.
