#!/bin/sh
# Boot orchid as PID 1 in qemu with an initramfs: a static orchid, bash, and a few services, one of
# which drives the `orchid` commands. Run as `unshare -r --map-auto tests/boot.sh ROOTDIR KERNEL`
# (ROOTDIR: a glibc system with /bin/bash and coreutils, for the services).
set -e
here=$(cd "$(dirname "$0")/.." && pwd)
root=${1:?usage: boot.sh ROOTDIR KERNEL}; kernel=${2:?}
orchid=$here/target/x86_64-unknown-linux-musl/release/orchid
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
t=$work/tree; mkdir -p "$t"/{sbin,bin,proc,sys,dev,tmp,lib,lib64,var/log,etc/orchid/services}
install -m755 "$orchid" "$t/init"; install -m755 "$orchid" "$t/sbin/orchid"
echo asteros > "$t/etc/hostname"; echo junk > "$t/tmp/old-file"
printf 'root:x:0:0:root:/root:/bin/bash\nnobody:x:65534:65534:nobody:/:/bin/false\n' > "$t/etc/passwd"
printf 'root:x:0:\nnobody:x:65534:\n' > "$t/etc/group"
for b in bash sleep ls cat head grep id; do
    for d in usr/bin bin; do [ -e "$root/$d/$b" ] && { cp -L "$root/$d/$b" "$t/bin/$b"; break; }; done
done
for b in "$t"/bin/*; do
    chroot "$root" /lib/ld-linux-x86-64.so.2 --list "/usr/bin/$(basename "$b")" 2>/dev/null || true
done | sed -n 's|.* => \(/[^ ]*\) .*|\1|p; s|^[[:space:]]*\(/[^ ]*ld-linux[^ ]*\) .*|\1|p' | sort -u | while read -r l; do
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
description = "a daemon that needs a"
exec = ["/bin/sleep", "3600"]
needs = ["a"]
S
cat > "$s/c.toml" <<S
[service]
exec = ["/bin/bash", "-c", "sleep 1; exit 1"]
restart = "always"
S
cat > "$s/d.toml" <<S
[service]
description = "runs as nobody, logs to a file"
exec = ["/bin/bash", "-c", "echo d is running as \$(id -un) with \$GREETING; exec sleep 3600"]
user = "nobody"
env = { GREETING = "hi" }
S
cat > "$s/lost.toml" <<S
[service]
exec = ["/bin/true"]
needs = ["nope"]
S
cat > "$s/probe.toml" <<'S'
[service]
description = "drives the orchid commands"
exec = ["/bin/bash", "-c", '''
exec 2>&1
sleep 4
echo "--- list"; orchid list
echo "--- status b"; orchid status b
echo "--- stop b"; orchid stop b; orchid list | grep '^b'
echo "--- start b"; orchid start b; orchid list | grep '^b'
echo "--- restart b"; orchid restart b; orchid list | grep '^b'
echo "--- disable d, enable d"; orchid disable d; orchid list | grep '^d'; ls /etc/orchid/enabled | tr '\n' ' '; echo; orchid enable d; orchid list | grep '^d'
echo "--- log d"; orchid log d
echo "--- errors"; orchid status nope; orchid start c lost; orchid bogus
echo "--- reload"; orchid reload
echo "--- version"; orchid version
echo "--- poweroff"; orchid poweroff
''']
after = ["b", "d"]
output = "console"
S
(cd "$t" && find . | cpio -o -H newc --quiet | gzip > "$work/initrd.gz")
timeout 120 qemu-system-x86_64 -machine q35 -m 512 -enable-kvm -cpu host -nographic -no-reboot \
    -kernel "$kernel" -initrd "$work/initrd.gz" -append "console=ttyS0 rdinit=/init panic=-1" 2>&1 | sed 's/\x1b\[[0-9;?]*[a-zA-Z]//g' | tr -d '\r'
