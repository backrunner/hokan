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

COPYFILE_DISABLE=1 tar -C dist -cf - "$package" | gzip -n > "$archive"
rm -r "$stage"
echo "$archive"
