#!/bin/sh
set -eu

repository=${HOKAN_REPOSITORY:-backrunner/hokan}
install_dir=${HOKAN_INSTALL_DIR:-${HOME:-}/.local/bin}
man_dir=${HOKAN_MAN_DIR:-${HOME:-}/.local/share/man/man1}
requested_version=${HOKAN_VERSION:-}
requested_shell=${HOKAN_SHELL:-}
requested_rc_file=${HOKAN_RC_FILE:-}
on_demand=${HOKAN_ON_DEMAND:-0}
release_base_override=${HOKAN_RELEASE_BASE_URL:-}

fail() {
  printf 'hokan installer: %s\n' "$*" >&2
  exit 1
}

say() {
  printf '%s\n' "$*"
}

[ -n "${HOME:-}" ] || fail 'HOME is not set'
command -v curl >/dev/null 2>&1 || fail 'curl is required'
command -v tar >/dev/null 2>&1 || fail 'tar is required'

case "$install_dir" in
  /*) ;;
  *) fail 'HOKAN_INSTALL_DIR must be an absolute path' ;;
esac
case "$man_dir" in
  /*) ;;
  *) fail 'HOKAN_MAN_DIR must be an absolute path' ;;
esac
if [ -z "$requested_shell" ]; then
  [ -n "${SHELL:-}" ] || fail 'SHELL is not set; set HOKAN_SHELL to zsh, bash, or fish'
  requested_shell=${SHELL##*/}
fi
case "$requested_shell" in
  zsh|bash|fish) ;;
  *) fail 'HOKAN_SHELL must be zsh, bash, or fish' ;;
esac
case "$on_demand" in
  0|1) ;;
  *) fail 'HOKAN_ON_DEMAND must be 0 or 1' ;;
esac

os=$(uname -s)
architecture=$(uname -m)
case "$os" in
  Darwin) platform=apple-darwin ;;
  Linux) platform=unknown-linux-gnu ;;
  *) fail "unsupported operating system: $os" ;;
esac
case "$architecture" in
  arm64|aarch64) architecture=aarch64 ;;
  x86_64|amd64) architecture=x86_64 ;;
  *) fail "unsupported architecture: $architecture" ;;
esac
target=$architecture-$platform

if [ -z "$requested_version" ]; then
  latest_url=$(curl \
    --proto '=https' \
    --tlsv1.2 \
    --fail \
    --silent \
    --show-error \
    --location \
    --output /dev/null \
    --write-out '%{url_effective}' \
    "https://github.com/$repository/releases/latest")
  latest_url=${latest_url%/}
  latest_tag=${latest_url##*/}
  case "$latest_tag" in
    v*) requested_version=${latest_tag#v} ;;
    *) fail "could not resolve the latest release from $latest_url" ;;
  esac
else
  requested_version=${requested_version#v}
fi
case "$requested_version" in
  ''|*[!0-9A-Za-z.-]*) fail "invalid version: $requested_version" ;;
esac

archive_name=hokan-$requested_version-$target.tar.gz
if [ -n "$release_base_override" ]; then
  release_base=${release_base_override%/}
else
  release_base=https://github.com/$repository/releases/download/v$requested_version
fi
temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/hokan-install.XXXXXX")
trap 'rm -rf "$temporary_directory"' EXIT HUP INT TERM
archive=$temporary_directory/$archive_name
checksums=$temporary_directory/SHA256SUMS

download() {
  if [ -n "$release_base_override" ]; then
    curl \
      --fail \
      --silent \
      --show-error \
      --location \
      --output "$2" \
      "$1"
  else
    curl \
      --proto '=https' \
      --tlsv1.2 \
      --fail \
      --silent \
      --show-error \
      --location \
      --output "$2" \
      "$1"
  fi
}

say "Downloading Hokan $requested_version for $target..."
download "$release_base/$archive_name" "$archive"
download "$release_base/SHA256SUMS" "$checksums"

expected_checksum=$(awk -v archive="$archive_name" '
  {
    name = $2
    sub(/^\*/, "", name)
    if (name == archive) {
      print $1
      exit
    }
  }
' "$checksums")
[ -n "$expected_checksum" ] || fail "$archive_name is missing from SHA256SUMS"

if command -v sha256sum >/dev/null 2>&1; then
  actual_checksum=$(sha256sum "$archive" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  actual_checksum=$(shasum -a 256 "$archive" | awk '{print $1}')
elif command -v openssl >/dev/null 2>&1; then
  actual_checksum=$(openssl dgst -sha256 "$archive" | awk '{print $NF}')
else
  fail 'sha256sum, shasum, or openssl is required to verify the release'
fi

expected_checksum=$(printf '%s' "$expected_checksum" | tr '[:upper:]' '[:lower:]')
actual_checksum=$(printf '%s' "$actual_checksum" | tr '[:upper:]' '[:lower:]')
[ "$actual_checksum" = "$expected_checksum" ] || fail 'SHA256 verification failed'

tar -xzf "$archive" -C "$temporary_directory"
package_directory=$temporary_directory/hokan-$requested_version-$target
source_binary=$package_directory/bin/hokan
source_man_page=$package_directory/share/man/man1/hokan.1
[ -x "$source_binary" ] || fail 'release archive does not contain bin/hokan'
[ -f "$source_man_page" ] || fail 'release archive does not contain hokan.1'

version_output=$("$source_binary" --version 2>&1) || fail 'downloaded binary failed its smoke test'
case "$version_output" in
  "hokan $requested_version") ;;
  *) fail "downloaded binary reported an unexpected version: $version_output" ;;
esac

mkdir -p "$install_dir" "$man_dir"
destination_binary=$install_dir/hokan
destination_man_page=$man_dir/hokan.1
temporary_binary=$install_dir/.hokan-install-$$
temporary_man_page=$man_dir/.hokan.1-install-$$

if [ -e "$destination_binary" ]; then
  [ -f "$destination_binary" ] && [ -x "$destination_binary" ] \
    || fail "$destination_binary exists but is not an executable file"
  if existing_version=$("$destination_binary" --version 2>/dev/null); then
    case "$existing_version" in
      "hokan "*) ;;
      *) fail "$destination_binary is not a Hokan executable" ;;
    esac
  else
    fail "$destination_binary exists but could not be verified as Hokan"
  fi
  cp "$destination_binary" "$destination_binary.bak"
fi
cp "$source_binary" "$temporary_binary"
chmod 0755 "$temporary_binary"
mv -f "$temporary_binary" "$destination_binary"
cp "$source_man_page" "$temporary_man_page"
chmod 0644 "$temporary_man_page"
mv -f "$temporary_man_page" "$destination_man_page"

set -- install --managed-install --man-page "$destination_man_page"
set -- "$@" --shell "$requested_shell"
if [ -n "$requested_rc_file" ]; then
  set -- "$@" --rc-file "$requested_rc_file"
fi
if [ "$on_demand" = 1 ]; then
  set -- "$@" --on-demand
fi
"$destination_binary" "$@"

say "Installed Hokan $requested_version"
say "  binary:  $destination_binary"
say "  man page: $destination_man_page"
say 'Open a new terminal session to start Hokan.'
