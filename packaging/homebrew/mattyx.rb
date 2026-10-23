# typed: false
# frozen_string_literal: true

# Homebrew formula for `mattyx` (`mtyx`) — issue #141.
#
# Rendered automatically by `scripts/render-homebrew-formula.sh <version> <dist_dir>`
# during each tagged release in `.github/workflows/release.yml`, and published
# as `mattyx.rb` alongside the release assets for the `homebrew-mattyx` tap.
class Mattyx < Formula
  desc "Agent-aware terminal multiplexer backed by libghostty-vt"
  homepage "https://github.com/mathewclarkau/mattyx"
  version "0.17.2"
  license "GPL-3.0-or-later"

  on_macos do
    on_arm do
      url "https://github.com/mathewclarkau/mattyx/releases/download/v#{version}/mtyx-macos-aarch64"
      sha256 "PLACEHOLDER_SHA256_MACOS_AARCH64"
    end
    on_intel do
      url "https://github.com/mathewclarkau/mattyx/releases/download/v#{version}/mtyx-macos-x86_64"
      sha256 "PLACEHOLDER_SHA256_MACOS_X86_64"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/mathewclarkau/mattyx/releases/download/v#{version}/mtyx-linux-aarch64"
      sha256 "PLACEHOLDER_SHA256_LINUX_AARCH64"
    end
    on_intel do
      url "https://github.com/mathewclarkau/mattyx/releases/download/v#{version}/mtyx-linux-x86_64"
      sha256 "PLACEHOLDER_SHA256_LINUX_X86_64"
    end
  end

  def install
    if OS.mac?
      bin.install(Hardware::CPU.arm? ? "mtyx-macos-aarch64" : "mtyx-macos-x86_64" => "mtyx")
    else
      bin.install(Hardware::CPU.arm? ? "mtyx-linux-aarch64" : "mtyx-linux-x86_64" => "mtyx")
    end
  end

  test do
    assert_equal "mtyx #{version}", shell_output("#{bin}/mtyx --version").strip
  end
end
