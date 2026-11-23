#!/usr/bin/env bash
set -euo pipefail

# Idempotent, non-interactive Linux (Ubuntu/Debian) build environment setup for OpenMirasim.
#
# Usage:
#   bash scripts/setup-env.sh [--cloud] [--desktop] [--android]
#
# Flags:
#   --cloud    Adapt ~/.bashrc for non-interactive cloud VM shells (used by .agents/environment.json).
#              By default, ~/.bashrc is NOT modified on local developer machines.
#   --desktop  Install Phase 3 Tauri v2 Linux desktop libraries and cargo-tauri CLI.
#   --android  Install Phase 4 Android NDK and Rust Android cross-compilation targets.

NDK_VERSION="28.2.13676358"
CLOUD_MODE=0
WITH_DESKTOP=0
WITH_ANDROID=0

for arg in "$@"; do
  case "$arg" in
    --cloud)   CLOUD_MODE=1 ;;
    --desktop) WITH_DESKTOP=1 ;;
    --android) WITH_ANDROID=1 ;;
    -h|--help)
      sed -n '3,14p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "Unknown option: $arg" >&2
      echo "Usage: bash scripts/setup-env.sh [--cloud] [--desktop] [--android]" >&2
      exit 2
      ;;
  esac
done

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  SUDO="sudo"
fi

mkdir -p "$HOME/.local/bin"

add_bashrc_line() {
  local line="$1"
  grep -qxF "$line" "$HOME/.bashrc" || echo "$line" >> "$HOME/.bashrc"
}

# 1. Cloud VM non-interactive shell PATH persistence (opt-in via --cloud only).
if [ "$CLOUD_MODE" -eq 1 ]; then
  touch "$HOME/.bashrc"
  sed -i '/case \$- in/,/esac/d' "$HOME/.bashrc" 2>/dev/null || true
  add_bashrc_line 'export PATH="$HOME/.local/bin:$PATH"'
  if [ "$WITH_ANDROID" -eq 1 ]; then
    add_bashrc_line 'export ANDROID_HOME="${ANDROID_HOME:-/opt/android-sdk}"'
    add_bashrc_line "export NDK_HOME=\"\$ANDROID_HOME/ndk/${NDK_VERSION}\""
    add_bashrc_line 'export ANDROID_NDK_HOME="$NDK_HOME"'
  fi
fi

export PATH="$HOME/.local/bin:${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"

# 2. Install required Ubuntu/Debian system packages (base + optional --desktop).
APT_PACKAGES=(
  build-essential
  clang
  lld
  pkg-config
  libssl-dev
  libsqlite3-dev
  sqlite3
  bubblewrap
  curl
  wget
  file
  git
)

if [ "$WITH_DESKTOP" -eq 1 ]; then
  APT_PACKAGES+=(
    libwebkit2gtk-4.1-dev
    libgtk-3-dev
    libayatana-appindicator3-dev
    librsvg2-dev
    libxdo-dev
    patchelf
    xvfb
  )
fi

if command -v apt-get >/dev/null 2>&1; then
  MISSING_PACKAGES=()
  for pkg in "${APT_PACKAGES[@]}"; do
    if ! dpkg-query -W -f='${Status}\n' "$pkg" 2>/dev/null | grep -qx 'install ok installed'; then
      MISSING_PACKAGES+=("$pkg")
    fi
  done

  if [ "${#MISSING_PACKAGES[@]}" -gt 0 ]; then
    $SUDO env DEBIAN_FRONTEND=noninteractive apt-get update
    $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${MISSING_PACKAGES[@]}"
  fi
fi

# 3. Validate base toolchains (Rust >= 1.85, Node >= 22, pnpm >= 10).
version_ge() {
  [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]
}

if ! command -v rustc >/dev/null 2>&1 || ! command -v cargo >/dev/null 2>&1; then
  echo "ERROR: rustc/cargo not found. Install Rust via https://rustup.rs/ (MSRV 1.85)." >&2
  exit 1
fi
RUST_VER="$(rustc --version | awk '{print $2}' | cut -d- -f1)"
if ! version_ge "$RUST_VER" "1.85.0"; then
  echo "ERROR: Rust >= 1.85.0 required, found $RUST_VER (run: rustup update stable)." >&2
  exit 1
fi

if ! command -v node >/dev/null 2>&1; then
  echo "ERROR: node not found. Install Node.js >= 22." >&2
  exit 1
fi
NODE_VER="$(node -v | sed 's/^v//')"
if ! version_ge "$NODE_VER" "22.0.0"; then
  echo "ERROR: Node.js >= 22.0.0 required, found $NODE_VER." >&2
  exit 1
fi

if ! command -v pnpm >/dev/null 2>&1; then
  echo "ERROR: pnpm not found. Enable via 'corepack enable' or install pnpm >= 10." >&2
  exit 1
fi
PNPM_VER="$(pnpm --version)"
if ! version_ge "$PNPM_VER" "10.0.0"; then
  echo "ERROR: pnpm >= 10.0.0 required, found $PNPM_VER." >&2
  exit 1
fi

# 4. Optional Phase 3: Tauri v2 CLI (architecture-aware cargo-binstall).
if [ "$WITH_DESKTOP" -eq 1 ]; then
  if ! command -v cargo-binstall >/dev/null 2>&1; then
    ARCH="$(uname -m)"
    case "$ARCH" in
      x86_64)  BINSTALL_TRIPLE="x86_64-unknown-linux-musl" ;;
      aarch64) BINSTALL_TRIPLE="aarch64-unknown-linux-musl" ;;
      *)
        echo "ERROR: Unsupported architecture for cargo-binstall: $ARCH" >&2
        exit 1
        ;;
    esac
    TMP_DIR="$(mktemp -d)"
    curl -fsSL "https://github.com/cargo-bins/cargo-binstall/releases/latest/download/cargo-binstall-${BINSTALL_TRIPLE}.tgz" \
      | tar -xz -C "$TMP_DIR"
    mv -f "$TMP_DIR/cargo-binstall" "$HOME/.local/bin/cargo-binstall"
    rm -rf "$TMP_DIR"
  fi

  if ! command -v cargo-tauri >/dev/null 2>&1; then
    cargo binstall -y --locked "tauri-cli@^2.0.0"
  fi
fi

# 5. Optional Phase 4: Android NDK & Rust Android cross-compilation targets.
if [ "$WITH_ANDROID" -eq 1 ]; then
  export ANDROID_HOME="${ANDROID_HOME:-/opt/android-sdk}"
  export NDK_HOME="$ANDROID_HOME/ndk/${NDK_VERSION}"
  export ANDROID_NDK_HOME="$NDK_HOME"
  export PATH="$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH"

  if command -v rustup >/dev/null 2>&1; then
    INSTALLED_TARGETS="$(rustup target list --installed)"
    ANDROID_TARGETS=(
      aarch64-linux-android
      armv7-linux-androideabi
      i686-linux-android
      x86_64-linux-android
    )
    MISSING_TARGETS=()
    for target in "${ANDROID_TARGETS[@]}"; do
      if ! printf '%s\n' "$INSTALLED_TARGETS" | grep -qx "$target"; then
        MISSING_TARGETS+=("$target")
      fi
    done
    if [ "${#MISSING_TARGETS[@]}" -gt 0 ]; then
      rustup target add "${MISSING_TARGETS[@]}"
    fi
  fi

  if command -v sdkmanager >/dev/null 2>&1 && [ ! -d "$NDK_HOME" ]; then
    (yes || true) | sdkmanager --licenses >/dev/null 2>&1 || true
    sdkmanager --install "ndk;${NDK_VERSION}"
  fi
fi

echo "OpenMirasim build environment ready (Rust ${RUST_VER}, Node ${NODE_VER}, pnpm ${PNPM_VER})."
