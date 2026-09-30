#!/usr/bin/env bash
# Verify downloaded macOS release archives before they are used in a formula.
set -euo pipefail

if [[ $# -ne 3 || ! "$3" =~ ^[A-Z0-9]{10}$ ]]; then
  printf 'usage: %s VERSION ASSET_DIRECTORY EXPECTED_APPLE_TEAM_ID\n' "$0" >&2
  exit 64
fi
[[ "$(uname -s)" == Darwin ]] || {
  printf 'release verification requires macOS Gatekeeper\n' >&2
  exit 1
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
version="$1"
asset_dir="$2"
expected_team_id="$3"
hashes="$("$script_dir/release-checksums.sh" "$version" "$asset_dir")"
verify_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-verify.XXXXXX")"
trap 'rm -r "$verify_root"' EXIT

fail() { printf 'release verification: %s\n' "$1" >&2; exit 1; }
team_id=''
for arch in x86_64 arm64; do
  if [[ "$arch" == arm64 ]]; then
    archive_arch=aarch64
  else
    archive_arch=x86_64
  fi
  archive="$asset_dir/reccursive-$version-darwin-$archive_arch.zip"
  destination="$verify_root/$arch"
  mkdir -p "$destination"
  ditto -x -k "$archive" "$destination"
  [[ -f "$destination/LICENSE" && ! -L "$destination/LICENSE" ]] ||
    fail "missing regular license: $archive_arch"
  for command_name in reccursive reccursive-daemon; do
    binary="$destination/$command_name"
    [[ -f "$binary" && ! -L "$binary" && -x "$binary" ]] ||
      fail "missing regular executable: $archive_arch/$command_name"
    [[ "$(lipo -archs "$binary")" == "$arch" ]] ||
      fail "wrong architecture: $archive_arch/$command_name"
    codesign --verify --deep --strict "$binary" ||
      fail "invalid code signature: $archive_arch/$command_name"
    signature="$(codesign -dv --verbose=4 "$binary" 2>&1)"
    grep -q '^Authority=Developer ID Application:' <<< "$signature" ||
      fail "not Developer ID signed: $archive_arch/$command_name"
    grep -q '^Timestamp=' <<< "$signature" ||
      fail "no secure signing timestamp: $archive_arch/$command_name"
    signed_team="$(sed -n 's/^TeamIdentifier=//p' <<< "$signature" | head -n 1)"
    [[ "$signed_team" == "$expected_team_id" ]] ||
      fail "unexpected signing team: $archive_arch/$command_name"
    if [[ -n "$team_id" && "$team_id" != "$signed_team" ]]; then
      fail "archives are signed by different teams"
    fi
    team_id="$signed_team"
    spctl -vvv --assess --type exec "$binary" ||
      fail "Gatekeeper rejected: $archive_arch/$command_name"
  done
done

printf '%s\n' "$hashes"
