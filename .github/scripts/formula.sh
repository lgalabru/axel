#!/usr/bin/env bash
# The Homebrew formula for one release: formula.sh VERSION OWNER/REPO ARM_SHA INTEL_SHA
set -euo pipefail
version=$1 repo=$2 arm=$3 intel=$4
base="https://github.com/$repo/releases/download/v$version"
cat <<RUBY
# Written by the release workflow in https://github.com/$repo; edits here are overwritten.
class Axel < Formula
  desc "Agent orchestration, designed for Rex"
  homepage "https://axel.md"
  version "$version"
  license "Apache-2.0"

  depends_on :macos

  on_arm do
    url "$base/axel-$version-aarch64-apple-darwin.tar.gz"
    sha256 "$arm"
  end
  on_intel do
    url "$base/axel-$version-x86_64-apple-darwin.tar.gz"
    sha256 "$intel"
  end

  def install
    bin.install "axel"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/axel --version")
  end
end
RUBY
