#!/usr/bin/env bash
# Render the Homebrew formula after the release workflow has produced its two
# SHA-256 values. The formula belongs in Aaryan1524/homebrew-tap, not this
# source repository, so it is deliberately emitted to stdout for review.
set -euo pipefail

if [[ $# -ne 3 ]]; then
  printf 'usage: %s VERSION X86_64_SHA256 AARCH64_SHA256\n' "$0" >&2
  exit 64
fi

version="$1"
x86_64_sha="$2"
aarch64_sha="$3"

cat <<FORMULA
class Reccursive < Formula
  desc "Queue verified Git changes for scheduled local publication"
  homepage "https://github.com/Aaryan1524/ReccursiveCommits"
  license "MIT"
  version "$version"

  on_intel do
    url "https://github.com/Aaryan1524/ReccursiveCommits/releases/download/v$version/reccursive-$version-darwin-x86_64.zip"
    sha256 "$x86_64_sha"
  end

  on_arm do
    url "https://github.com/Aaryan1524/ReccursiveCommits/releases/download/v$version/reccursive-$version-darwin-aarch64.zip"
    sha256 "$aarch64_sha"
  end

  def install
    bin.install "reccursive", "reccursive-daemon"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/reccursive --version")
  end
end
FORMULA
