class Ninmu < Formula
  desc "Agentic AI coding assistant for the terminal"
  homepage "https://ninmu.dev"
  version "VERSION"

  on_macos do
    on_arm do
      url "https://github.com/deep-thinking-llc/claw-code/releases/download/vVERSION/ninmu-macos-arm64"
      sha256 "ARM64_SHA256"
    end
    on_intel do
      url "https://github.com/deep-thinking-llc/claw-code/releases/download/vVERSION/ninmu-macos-x64"
      sha256 "X64_SHA256"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/deep-thinking-llc/claw-code/releases/download/vVERSION/ninmu-linux-arm64"
      sha256 "LINUX_ARM64_SHA256"
    end
    on_intel do
      url "https://github.com/deep-thinking-llc/claw-code/releases/download/vVERSION/ninmu-linux-x64"
      sha256 "LINUX_X64_SHA256"
    end
  end

  def install
    bin.install "ninmu"
  end

  test do
    assert_match "ninmu", shell_output("#{bin}/ninmu --version")
  end
end
