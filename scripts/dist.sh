#!/bin/sh
# The release tarball NAME-VERSION.tar.xz: the committed sources and their vendored crates, so that
# a recipe builds it without a network (the build sandbox has none). Run in the repository.
set -e
cd "$(dirname "$0")/.."
name=$(sed -n 's/^name = "\(.*\)"/\1/p' Cargo.toml | head -1)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
out=${1:-$PWD/dist}
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
dir=$tmp/$name-$version
mkdir -p "$dir" "$out"
git archive HEAD | tar -x -C "$dir"
(cd "$dir" && mkdir .cargo && cargo vendor --locked vendor > .cargo/config.toml)
tar -C "$tmp" --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 -cf - "$name-$version" | xz -9 -T0 > "$out/$name-$version.tar.xz"
sha256sum "$out/$name-$version.tar.xz"
