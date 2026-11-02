#!/bin/sh
# Install Axel, agent orchestration designed for Rex.
#
#   curl -fsSL https://axel.md/install | sh
#   curl -fsSL https://axel.md/install | sh -s -- 0.3.80    # a given version
#
# Downloads the latest release of https://github.com/scarce/axel for this Mac,
# checks its sha256 (and its GitHub build provenance, when `gh` is logged in),
# then installs `axel` into ~/.local/bin, or $AXEL_INSTALL_DIR. Nothing else on
# the system is touched, and nothing needs sudo. Run it again to update: every
# commit on main that passes CI is a release.
#
# The whole script is one function called on the last line, so a download cut
# short runs nothing.

main() {
  set -eu
  umask 022

  repo="scarce/axel"
  dir="${AXEL_INSTALL_DIR:-$HOME/.local/bin}"

  if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    bold=$(printf '\033[1m') dim=$(printf '\033[2m') green=$(printf '\033[32m') red=$(printf '\033[31m') reset=$(printf '\033[0m')
  else
    bold='' dim='' green='' red='' reset=''
  fi

  for tool in curl tar shasum uname mktemp; do
    command -v "$tool" >/dev/null 2>&1 || die "needs \`$tool\`"
  done
  [ "$(uname -s)" = Darwin ] || die "Axel runs on macOS, next to Rex."
  [ "$(id -u)" != 0 ] || die "run this as yourself, not root: it installs into your home"

  case "$(uname -m)" in
    arm64) target=aarch64-apple-darwin ;;
    x86_64)
      # A shell under Rosetta reports x86_64 on Apple silicon: take the native build.
      if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
        target=aarch64-apple-darwin
      else
        target=x86_64-apple-darwin
      fi ;;
    *) die "unsupported CPU: $(uname -m)" ;;
  esac

  if [ $# -gt 0 ]; then
    version=${1#v}
  else
    # /releases/latest redirects to /releases/tag/v<version>.
    latest=$(fetch -I -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") \
      || die "could not reach github.com"
    case "$latest" in
      */releases/tag/v*) version=${latest##*/tag/v} ;;
      *) die "no release published yet: https://github.com/$repo/releases" ;;
    esac
  fi
  case "$version" in
    '' | *[!0-9.]* | .* | *.) die "not a version: $version" ;;
  esac

  name="axel-$version-$target.tar.gz"
  url="https://github.com/$repo/releases/download/v$version/$name"
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/axel-install.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT
  trap 'exit 130' INT TERM

  step "Downloading axel ${bold}$version${reset} ${dim}($target)${reset}"
  fetch -o "$tmp/$name" "$url" || die "no build for $target in v$version ($url)"
  fetch -o "$tmp/$name.sha256" "$url.sha256" || die "no checksum for $name"

  want=$(cut -d' ' -f1 <"$tmp/$name.sha256")
  got=$(shasum -a 256 "$tmp/$name" | cut -d' ' -f1)
  [ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $name: refusing to install"
  ok "Checksum verified"

  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    gh attestation verify "$tmp/$name" --repo "$repo" >/dev/null 2>&1 \
      || die "build provenance check failed for $name: refusing to install"
    ok "Built by github.com/$repo, provenance verified"
  fi

  # The archive holds one file, `axel`; anything else means it isn't ours.
  [ "$(tar -tzf "$tmp/$name")" = axel ] || die "unexpected contents in $name"
  tar -xzf "$tmp/$name" -C "$tmp" axel
  [ -f "$tmp/axel" ] && [ ! -L "$tmp/axel" ] || die "unexpected contents in $name"
  chmod 755 "$tmp/axel"
  installed=$("$tmp/axel" --version 2>/dev/null) || die "the downloaded binary does not run on this Mac"

  # Stage next to the destination, then rename: atomic, and a running axel keeps
  # its old binary. A symlink there (a source build's) is replaced, not followed.
  mkdir -p "$dir"
  cp "$tmp/axel" "$dir/.axel.$$"
  mv -f "$dir/.axel.$$" "$dir/axel"
  ok "Installed ${bold}$installed${reset} in $(tilde "$dir")"

  case ":$PATH:" in
    *":$dir:"*)
      first=$(command -v axel 2>/dev/null || true)
      if [ -n "$first" ] && [ "$first" != "$dir/axel" ]; then
        note "$(tilde "$first") comes first on your PATH; remove it to use this one."
      fi ;;
    *)
      # The line to add says $HOME, not ~: a quoted ~ never expands.
      path=$(homevar "$dir")
      # shellcheck disable=SC2088 # rc is only displayed
      case "${SHELL:-}" in
        */zsh) rc="~/.zshrc" line="export PATH=\"$path:\$PATH\"" ;;
        */bash) rc="~/.bash_profile" line="export PATH=\"$path:\$PATH\"" ;;
        */fish) rc="~/.config/fish/config.fish" line="fish_add_path \"$path\"" ;;
        *) rc="your shell's profile" line="export PATH=\"$path:\$PATH\"" ;;
      esac
      note "$(tilde "$dir") is not on your PATH. Add this line to $rc:"
      printf '\n    %s\n' "$line" ;;
  esac

  printf '\nRun %saxel%s to get started.\n' "$bold" "$reset"
}

# HTTPS only (redirects too), TLS 1.2 or newer, retries on flaky networks, and a
# failure on any HTTP error, which the caller reports in its own words.
fetch() { curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --silent --location --retry 3 "$@"; }

step() { printf '%s\n' "$*"; }
ok() { printf '%s✓%s %s\n' "$green" "$reset" "$*"; }
note() { printf '%s!%s %s\n' "$bold" "$reset" "$*"; }
die() { printf '%saxel install:%s %s\n' "$red" "$reset" "$*" >&2; exit 1; }
# A path as shown to you (~/.local/bin), and as written into a profile ($HOME/.local/bin).
# shellcheck disable=SC2088
tilde() { case "$1" in "$HOME"/*) printf '~/%s' "${1#"$HOME"/}" ;; *) printf '%s' "$1" ;; esac; }
# shellcheck disable=SC2016
homevar() { case "$1" in "$HOME"/*) printf '$HOME/%s' "${1#"$HOME"/}" ;; *) printf '%s' "$1" ;; esac; }

main "$@"
