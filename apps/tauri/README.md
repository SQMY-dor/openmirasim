# OpenMirasim Desktop Thin Shell (Tauri v2)

Cross-platform desktop client (**Linux / macOS / Windows**) wrapping the shared `apps/web` workbench with native desktop capabilities:

- **System Tray (`tray-icon`)**: quick show/hide toggle, pending approval tooltip badge, and background resident mode.
- **OS Keychain Token Storage (`keyring`)**: stores device tokens (`omt_...`) securely in macOS Keychain / Windows Credential Manager / Linux Secret Service (never writes tokens in plaintext to disk).
- **Multi-Server Profiles**: switch between dev/staging/production `om-server` hosts cleanly.
- **Native OS Notifications (`tauri-plugin-notification`)**: surfaces pending agent tool approvals directly to the OS notification center.

## Prerequisites

- Linux: run `./scripts/setup-env.sh --desktop` (installs `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`).
- macOS / Windows: standard Rust + Node.js 22 + pnpm toolchain, plus `cargo install tauri-cli --version "^2"`.

## Development & Packaging

```bash
# Terminal 1: start shared Web frontend dev server
pnpm --filter @openmirasim/web dev

# Terminal 2: start Tauri v2 desktop thin shell
pnpm --filter @openmirasim/tauri dev

# Production desktop installer (.AppImage / .deb / .dmg / .msi)
pnpm --filter @openmirasim/tauri build
```
