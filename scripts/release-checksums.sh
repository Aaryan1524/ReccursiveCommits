#!/usr/bin/env bash
# Validate a downloaded release's exact assets and emit its Intel and ARM SHA-256 values.
set -euo pipefail

if [[ $# -ne 2 || ! "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf 'usage: %s VERSION ASSET_DIRECTORY\n' "$0" >&2
  exit 64
fi

version="$1"
asset_dir="$2"
x86_name="reccursive-$version-darwin-x86_64.zip"
arm_name="reccursive-$version-darwin-aarch64.zip"
manifest="$asset_dir/SHA256SUMS"

fail() { printf 'release assets: %s\n' "$1" >&2; exit 1; }

[[ -d "$asset_dir" && -f "$manifest" && ! -L "$manifest" ]] ||
  fail 'SHA256SUMS is missing or is not a regular file'

x86_sha=''
arm_sha=''
entries=0
while read -r hash name extra; do
  [[ "$hash" =~ ^[[:xdigit:]]{64}$ && -z "${extra:-}" ]] ||
    fail 'SHA256SUMS contains a malformed entry'
  case "$name" in
    "$x86_name")
      [[ -z "$x86_sha" ]] || fail 'duplicate Intel checksum'
      x86_sha="$hash"
      ;;
    "$arm_name")
      [[ -z "$arm_sha" ]] || fail 'duplicate Apple-silicon checksum'
      arm_sha="$hash"
      ;;
    *) fail "unexpected asset in SHA256SUMS: $name" ;;
  esac
  entries=$((entries + 1))
done < "$manifest"

[[ "$entries" -eq 2 && -n "$x86_sha" && -n "$arm_sha" ]] ||
  fail 'SHA256SUMS must name exactly the Intel and Apple-silicon archives'

for name in "$x86_name" "$arm_name"; do
  [[ -f "$asset_dir/$name" && ! -L "$asset_dir/$name" ]] ||
    fail "missing regular archive: $name"
done

(cd "$asset_dir" && shasum -a 256 -c SHA256SUMS) >&2 ||
  fail 'an archive does not match SHA256SUMS'

for name in "$x86_name" "$arm_name"; do
  diff -u \
    <(printf 'LICENSE\nreccursive\nreccursive-daemon\n') \
    <(unzip -Z -1 "$asset_dir/$name" | LC_ALL=C sort) >&2 ||
    fail "unexpected ZIP contents: $name"
done

printf '%s\n%s\n' "$x86_sha" "$arm_sha"
