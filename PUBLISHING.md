# Distributing & publishing GetAIBD

The extension bundles a native `getaibd-agent` engine, so it ships as
**platform-specific VSIXes** (one per OS/arch), not a single universal package.

| vsce `--target` | Rust triple |
|---|---|
| `darwin-arm64` | `aarch64-apple-darwin` |
| `darwin-x64`   | `x86_64-apple-darwin` |
| `win32-x64`    | `x86_64-pc-windows-msvc` |
| `linux-x64`    | `x86_64-unknown-linux-gnu` |
| `linux-arm64`  | `aarch64-unknown-linux-gnu` |

## 1. Build the VSIXes

```bash
scripts/package-all.sh            # all targets the host can build -> dist-vsix/
scripts/package-all.sh darwin-arm64   # one target
```

Cross-OS builds need the right toolchain; CI (`.github/workflows/release.yml`)
builds the full matrix on macOS/Windows/Linux runners.

## 2a. Marketplaces (managed install + auto-update)

- **VS Code Marketplace** → VS Code users. Needs publisher `dmsbilas` and an
  Azure DevOps PAT (scope: Marketplace → Manage) as `VSCE_PAT`.
- **Open VSX** → Cursor / VSCodium / Windsurf users. Needs an open-vsx.org
  namespace `dmsbilas` and token as `OVSX_PAT`.

```bash
for f in dist-vsix/*.vsix; do
  npx @vscode/vsce publish --packagePath "$f" --pat "$VSCE_PAT"
  npx ovsx publish "$f" -p "$OVSX_PAT"
done
```

## 2b. Direct download (no marketplace)

Upload `dist-vsix/*.vsix` to R2 / a `getaibd.com/download` page or GitHub
Releases. Users install with:

```bash
code --install-extension getaibd-darwin-arm64-0.1.0.vsix
```

(or drag the file into the Extensions view). No auto-update on this channel.

## macOS signing/notarization

Set before running `scripts/package-all.sh` on macOS:

```bash
export SIGN_IDENTITY="Developer ID Application: <Name> (TEAMID)"
export APPLE_ID=...  APPLE_APP_PASSWORD=...  APPLE_TEAM_ID=...
```

## CI release (tag-driven)

`git tag v0.1.1 && git push --tags` runs the matrix build, then on `v*` tags
uploads every VSIX to a GitHub Release and publishes to both marketplaces.

**Repo secrets:** `VSCE_PAT`, `OVSX_PAT`, `APPLE_CERT_P12_BASE64`,
`APPLE_CERT_PASSWORD`, `APPLE_SIGN_IDENTITY`, `APPLE_ID`, `APPLE_APP_PASSWORD`,
`APPLE_TEAM_ID`.
