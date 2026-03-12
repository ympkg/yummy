class Ym < Formula
  desc "Modern Java build tool - Gradle replacement with Yarn + Vite experience"
  homepage "https://github.com/ympkg/yummy"
  version "0.3.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/ympkg/yummy/releases/download/v#{version}/ym-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "PLACEHOLDER_SHA256_ARM64"
    end
    on_intel do
      url "https://github.com/ympkg/yummy/releases/download/v#{version}/ym-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "PLACEHOLDER_SHA256_X64"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/ympkg/yummy/releases/download/v#{version}/ym-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "PLACEHOLDER_SHA256_LINUX_ARM64"
    end
    on_intel do
      url "https://github.com/ympkg/yummy/releases/download/v#{version}/ym-#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "PLACEHOLDER_SHA256_LINUX_X64"
    end
  end

  def install
    bin.install "ym"
    bin.install "ym" => "ymc"
    lib.install "ym-agent.jar" if File.exist?("ym-agent.jar")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/ym --version")
  end
end
