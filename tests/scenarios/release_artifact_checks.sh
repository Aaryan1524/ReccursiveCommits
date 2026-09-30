#!/usr/bin/env bash
# Release hashes must come from the actual archives, not manually copied text.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
verifier="$project_root/scripts/release-checksums.sh"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-release-assets.XXXXXX")"
trap 'rm -r "$fixture_root"' EXIT

version=0.1.0
x86_name="reccursive-$version-darwin-x86_64.zip"
arm_name="reccursive-$version-darwin-aarch64.zip"
mkdir -p "$fixture_root/stage"
printf 'license\n' > "$fixture_root/stage/LICENSE"
printf 'cli\n' > "$fixture_root/stage/reccursive"
printf 'daemon\n' > "$fixture_root/stage/reccursive-daemon"

(
  cd "$fixture_root/stage"
  zip -q "$fixture_root/$x86_name" LICENSE reccursive reccursive-daemon
)
cp "$fixture_root/$x86_name" "$fixture_root/$arm_name"
write_checksums() {
  (cd "$fixture_root" && shasum -a 256 "$x86_name" "$arm_name" > SHA256SUMS)
}
reject() {
  if "$verifier" "$version" "$fixture_root" >/dev/null 2>&1; then
    printf 'FAIL: release verifier accepted %s\n' "$1" >&2
    exit 1
  fi
}

write_checksums
hashes="$("$verifier" "$version" "$fixture_root")"
expected_hashes="$(cd "$fixture_root" && shasum -a 256 "$x86_name" "$arm_name" | awk '{print $1}')"
[[ "$hashes" == "$expected_hashes" ]] ||
  { printf 'FAIL: expected the two archive hashes in architecture order\n' >&2; exit 1; }

printf 'tamper\n' >> "$fixture_root/$x86_name"
reject 'a modified archive'
cp "$fixture_root/$arm_name" "$fixture_root/$x86_name"
write_checksums

printf 'stray\n' > "$fixture_root/stage/stray.txt"
(
  cd "$fixture_root/stage"
  zip -q "$fixture_root/$x86_name" stray.txt
)
write_checksums
reject 'an unexpected ZIP entry'
cp "$fixture_root/$arm_name" "$fixture_root/$x86_name"
write_checksums

printf '%s\n' "$(head -n 1 "$fixture_root/SHA256SUMS")" >> "$fixture_root/SHA256SUMS"
reject 'a duplicate manifest entry'
write_checksums

mv "$fixture_root/$x86_name" "$fixture_root/original.zip"
ln -s original.zip "$fixture_root/$x86_name"
reject 'a symbolic-link archive'
printf 'ok\n'
