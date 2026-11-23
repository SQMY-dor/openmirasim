#!/usr/bin/env bash
set -euo pipefail

# Idempotent, non-interactive Linux (Ubuntu 24.04) build environment setup for OpenMirasim.
# Installs Tauri v2 system libraries, Rust/SQLite/lld build acceleration, Protobuf/Buf,
# Tauri v2 CLI, and Android NDK + Rust cross-compilation targets.

NDK_VERSION="28.2.13676358"
BUF_VERSION="1.59.0"

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  SUDO="sudo"
fi

mkdir -p "$HOME/.local/bin"
touch "$HOME/.bashrc"

# 1. Persist PATH and Android/NDK variables in ~/.bashrc for non-interactive shells too.
sed -i '/case \$- in/,/esac/d' "$HOME/.bashrc" 2>/dev/null || true

add_bashrc_line() {
  local line="$1"
  grep -qxF "$line" "$HOME/.bashrc" || echo "$line" >> "$HOME/.bashrc"
}

add_bashrc_line 'export PATH="$HOME/.local/bin:$PATH"'
add_bashrc_line 'export ANDROID_HOME="${ANDROID_HOME:-/opt/android-sdk}"'
add_bashrc_line "export NDK_HOME=\"\$ANDROID_HOME/ndk/${NDK_VERSION}\""
add_bashrc_line 'export ANDROID_NDK_HOME="$NDK_HOME"'

export ANDROID_HOME="${ANDROID_HOME:-/opt/android-sdk}"
export NDK_HOME="$ANDROID_HOME/ndk/${NDK_VERSION}"
export ANDROID_NDK_HOME="$NDK_HOME"
export PATH="$HOME/.local/bin:${CARGO_HOME:-$HOME/.cargo}/bin:$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH"

# 2. Install required Ubuntu/Debian system packages if any are missing.
APT_PACKAGES=(
  libwebkit2gtk-4.1-dev
  libgtk-3-dev
  libayatana-appindicator3-dev
  librsvg2-dev
  libxdo-dev
  patchelf
  build-essential
  pkg-config
  libssl-dev
  libsqlite3-dev
  sqlite3
  lld
  protobuf-compiler
  libprotobuf-dev
  xvfb
  curl
  wget
  file
)

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

# 3. Install Rust Android cross-compilation targets.
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

# 4. Install Android NDK (Side by side).
if command -v sdkmanager >/dev/null 2>&1 && [ ! -d "$NDK_HOME" ]; then
  (yes || true) | sdkmanager --licenses >/dev/null 2>&1 || true
  sdkmanager --install "ndk;${NDK_VERSION}"
fi

# 5. Install Buf CLI for Protobuf lint/breaking/codegen.
if ! command -v buf >/dev/null 2>&1 || [ "$(buf --version 2>/dev/null || true)" != "$BUF_VERSION" ]; then
  TMP_BUF="$(mktemp "$HOME/.local/bin/.buf.XXXXXX")"
  curl -fsSL "https://github.com/bufbuild/buf/releases/download/v${BUF_VERSION}/buf-Linux-x86_64" -o "$TMP_BUF"
  chmod +x "$TMP_BUF"
  mv -f "$TMP_BUF" "$HOME/.local/bin/buf"
fi

# 6. Install cargo-binstall and Tauri v2 CLI.
if ! command -v cargo-binstall >/dev/null 2>&1; then
  TMP_DIR="$(mktemp -d)"
  curl -fsSL "https://github.com/cargo-bins/cargo-binstall/releases/latest/download/cargo-binstall-x86_64-unknown-linux-musl.tgz" \
    | tar -xz -C "$TMP_DIR"
  mv -f "$TMP_DIR/cargo-binstall" "$HOME/.local/bin/cargo-binstall"
  rm -rf "$TMP_DIR"
fi

if ! command -v cargo-tauri >/dev/null 2>&1; then
  cargo binstall -y --locked "tauri-cli@^2.0.0"
fi

# 7. Configure Cargo to use lld linker on Linux x86_64 hosts.
if command -v clang >/dev/null 2>&1 && command -v ld.lld >/dev/null 2>&1; then
  CARGO_CONFIG="${CARGO_HOME:-$HOME/.cargo}/config.toml"
  mkdir -p "$(dirname "$CARGO_CONFIG")"
  if ! grep -q '^\[target\.x86_64-unknown-linux-gnu\]' "$CARGO_CONFIG" 2>/dev/null; then
    cat >> "$CARGO_CONFIG" <<'EOF'
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=lld"]
EOF
  fi
fi

echo "OpenMirasim build environment ready."
