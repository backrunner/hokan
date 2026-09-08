#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <target> <version>" >&2
  exit 2
fi

target=$1
version=$2
case "$target" in
  aarch64-apple-darwin|aarch64-unknown-linux-gnu|x86_64-apple-darwin|x86_64-unknown-linux-gnu) ;;
  *)
    echo "unsupported release target: $target" >&2
    exit 2
    ;;
esac
case "$version" in
  ""|*[!0-9A-Za-z._-]*)
    echo "invalid release version: $version" >&2
    exit 2
    ;;
esac

binary="target/$target/release/hokan"
if [[ ! -x "$binary" ]]; then
  echo "release binary is missing or not executable: $binary" >&2
  exit 1
fi

package="hokan-$version-$target"
stage="dist/$package"
archive="dist/$package.tar.gz"
if [[ -e "$stage" || -e "$archive" ]]; then
  echo "refusing to overwrite existing release output: $package" >&2
  exit 1
fi

install -d "$stage/bin" "$stage/share/man/man1"
install -m 0755 "$binary" "$stage/bin/hokan"
install -m 0644 README.md LICENSE "$stage/"
install -m 0644 docs/hokan.1 "$stage/share/man/man1/hokan.1"

# Older updaters only read the literal bin/hokan entry. Emit its contents
# first, then the installer layout with a hard link to that same binary.
# This preserves both layouts without doubling the archive's binary payload.
compat_stage=$(mktemp -d "dist/.hokan-package.XXXXXX")
trap 'rm -rf "$compat_stage"' EXIT
install -d "$compat_stage/bin"
ln "$stage/bin/hokan" "$compat_stage/bin/hokan"
COPYFILE_DISABLE=1 tar -C "$compat_stage" -cf - bin/hokan -C .. "$package" | gzip -n > "$archive"
rm -r "$stage"
echo "$archive"
