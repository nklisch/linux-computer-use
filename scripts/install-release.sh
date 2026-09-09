#!/bin/sh
# Install the latest release binary of linux-computer-use for the current user.
# Never changes system packages, desktop permissions, or running services.
#
# One-liner:
#   curl -fsSL https://raw.githubusercontent.com/nklisch/linux-computer-use/main/scripts/install-release.sh | sh
#
# Override the install prefix (default ~/.local) with LCU_INSTALL_ROOT.
set -eu

repo="nklisch/linux-computer-use"
prefix=${LCU_INSTALL_ROOT:-$HOME/.local}

case "$(uname -m)" in
  x86_64) target="x86_64-unknown-linux-gnu" ;;
  *)
    echo "No prebuilt release binary for $(uname -m)."
    echo "Build from source instead: https://github.com/${repo}#install"
    exit 1
    ;;
esac

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required." >&2
  exit 1
fi

tag=$(curl -fsSL "https://api.github.com/repos/${repo}/releases/latest" \
  | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')
if [ -z "${tag:-}" ]; then
  echo "Could not resolve the latest release." >&2
  exit 1
fi

name="lcu-${tag}-${target}"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading ${name}..."
curl -fsSL -o "$tmp/${name}.tar.gz" \
  "https://github.com/${repo}/releases/download/${tag}/${name}.tar.gz"
curl -fsSL -o "$tmp/${name}.tar.gz.sha256" \
  "https://github.com/${repo}/releases/download/${tag}/${name}.tar.gz.sha256" || true
if [ -f "$tmp/${name}.tar.gz.sha256" ]; then
  (cd "$tmp" && sha256sum -c "${name}.tar.gz.sha256")
fi

tar -xzf "$tmp/${name}.tar.gz" -C "$tmp"
mkdir -p "$prefix/bin"
install -m 755 "$tmp/${name}/lcu" "$prefix/bin/lcu"

echo "Installed: $("$prefix/bin/lcu" --version)"
case ":$PATH:" in
  *":$prefix/bin:"*) ;;
  *) echo "Note: $prefix/bin is not on your PATH." ;;
esac
echo "Next: lcu doctor   (checks portal/capture prerequisites; requests no desktop access)"
echo "MCP: register a stdio server with command 'lcu' and args [\"mcp\"]."
