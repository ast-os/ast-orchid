#!/bin/sh
# Boot orchid as PID 1 in qemu: an initramfs with a static orchid, bash and a few test services.
# Run as `unshare -r tests/boot.sh ROOTDIR KERNEL` (ROOTDIR: any glibc root with /bin/bash, for the services).
set -e
here=$(cd "$(dirname "$0")/.." && pwd)
root=${1:?usage: boot.sh ROOTDIR KERNEL}; kernel=${2:?}
orchid=$here/target/x86_64-unknown-linux-musl/release/orchid
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
t=$work/tree; mkdir -p "$t/sbin" "$t/bin" "$t/etc/orchid/services" "$t/proc" "$t/sys" "$t/dev" "$t/tmp" "$t/lib" "$t/lib64"
install -m755 "$orchid" "$t/init"; install -m755 "$orchid" "$t/sbin/orchid"
echo asteros > "$t/etc/hostname"; echo junk > "$t/tmp/old-file"
cp -L "$root/bin/bash.bash" "$t/bin/bash" 2>/dev/null || cp -L "$root/bin/bash" "$t/bin/bash"
for l in $(chroot "$root" /lib/ld-linux-x86-64.so.2 --list /bin/bash | sed -n 's|.* => \(/[^ ]*\) .*|\1|p; s|^[[:space:]]*\(/[^ ]*ld-linux[^ ]*\) .*|\1|p'); do
    mkdir -p "$t$(dirname "$l")"; cp -L "$root$l" "$t$l"
done
ln -snf /lib/ld-linux-x86-64.so.2 "$t/lib64/ld-linux-x86-64.so.2"
s=$t/etc/orchid/services
cat > "$s/a.toml" <<S
[service]
description = "a oneshot"
exec = ["/bin/bash", "-c", "echo hello from a"]
type = "oneshot"
S
cat > "$s/b.toml" <<S
[service]
exec = ["/bin/bash", "-c", "sleep 3600"]
needs = ["a"]
S
cat > "$s/c.toml" <<S
[service]
exec = ["/bin/bash", "-c", "sleep 1; exit 1"]
restart = "always"
S
cat > "$s/lost.toml" <<S
[service]
exec = ["/bin/true"]
needs = ["nope"]
S
cat > "$s/stop.toml" <<S
[service]
exec = ["/bin/bash", "-c", "sleep 12; /sbin/orchid poweroff"]
S
(cd "$t" && find . | cpio -o -H newc --quiet | gzip > "$work/initrd.gz")
timeout 90 qemu-system-x86_64 -machine q35 -m 512 -enable-kvm -cpu host -nographic -no-reboot \
    -kernel "$kernel" -initrd "$work/initrd.gz" -append "console=ttyS0 rdinit=/init panic=-1" 2>&1 | sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' | tr -d '\r'
