#!/usr/bin/env bash
# Download and validate the published release before rendering its tap formula.
# Only the formula is written to stdout; all diagnostics go to stderr.
set -euo pipefail

if [[ $# -ne 2 || ! "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ || ! "$2" =~ ^[A-Z0-9]{10}$ ]]; then
  printf 'usage: %s VERSION EXPECTED_APPLE_TEAM_ID\n' "$0" >&2
  exit 64
fi
[[ "$(uname -s)" == Darwin ]] || {
  printf 'formula preparation requires macOS Gatekeeper\n' >&2
  exit 1
}

version="$1"
expected_team_id="$2"
tag="v$version"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
release_repo=Aaryan1524/ReccursiveCommits
published_tag="$(gh release view "$tag" --repo "$release_repo" \
  --json tagName,isDraft,isPrerelease \
  --jq 'if .isDraft or .isPrerelease then "" else .tagName end')"
[[ "$published_tag" == "$tag" ]] || {
  printf 'release %s is absent, draft, or a prerelease\n' "$tag" >&2
  exit 1
}

download_dir="$(mktemp -d "${TMPDIR:-/tmp}/reccursive-release-download.XXXXXX")"
trap 'rm -r "$download_dir"' EXIT
gh release download "$tag" --repo "$release_repo" --dir "$download_dir" \
  --pattern SHA256SUMS --pattern 'reccursive-*.zip' >&2

hashes="$("$script_dir/verify-macos-release.sh" "$version" "$download_dir" "$expected_team_id")"
x86_sha="$(printf '%s\n' "$hashes" | sed -n '1p')"
arm_sha="$(printf '%s\n' "$hashes" | sed -n '2p')"
"$script_dir/render-homebrew-formula.sh" "$version" "$x86_sha" "$arm_sha"
