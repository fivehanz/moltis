class Moltis < Formula
  desc "Rust-powered bot framework with LLM agents, plugins, and gateway"
  homepage "https://github.com/penso/moltis"
  url "https://github.com/penso/moltis.git",
      tag:      "v0.1.0",
      revision: ""
  license "MIT"
  head "https://github.com/penso/moltis.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/cli")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/moltis --version", 2)
  end
end
