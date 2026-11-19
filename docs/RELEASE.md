# Releasing

> Current application identity: [APP-IDENTITY.md](APP-IDENTITY.md). The native
> implementation now ships only as mtty. Earlier dual-host rehearsals below
> are historical. The native app supports signed downloads and Install and
> Relaunch; current platform results are in [ACCEPTANCE.md](ACCEPTANCE.md).

How the release pipeline works, what to configure, and how the update chain fits
together. English default; keep [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md) in sync.

## Trigger

Push a tag:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

[`.github/workflows/release.yml`](../.github/workflows/release.yml) builds on
macOS, Linux and Windows, packages each platform, optionally signs, uploads the
artifacts, builds an **update manifest**, and creates a GitHub Release.

For a rehearsal without publishing a release:

```sh
gh workflow run release.yml --ref main -f tag=v0.0.1
```

Manual runs execute the same packaging, signing, and manifest checks, and upload
the combined `release-assembled` artifact. The manifest uses the supplied tag;
its download URLs become live only after a release with that tag is published.

## Release notes

**Release bodies are English, and always link to the Simplified Chinese
version.** The same rule applies to every miao project, including `miao`.

- The English notes are `docs/releases/<tag>.md`; the Chinese mirror is
  `docs/releases/<tag>.zh-CN.md`. Write both before tagging.
- `docs/releases/<tag>.md` starts with `[简体中文](<tag>.zh-CN.md)` right under
  the `# mtty <tag>` heading, and may link to other docs relatively.
- `scripts/release-notes.py` turns the file into the release body: it makes
  every relative link absolute at the tag and forces the `[简体中文]` link to
  point at the mirrored file. The release job prefers it and falls back to
  `--generate-notes` only when a tag has no notes file.
- Do not paste the Chinese notes into the release body; the body is English and
  links across. `scripts/test-release-notes.py` covers the transformation.

## Artifacts

| Platform | Artifact | Notes |
|----------|----------|-------|
| macOS | `mtty-macos-arm64.zip`, `mtty-macos-x86_64.zip` (each contains only `mtty.app`) | ad-hoc signed; notarized when the Apple secrets are set. Apple Silicon uses `macos-latest`, Intel uses `macos-15-intel` |
| Linux | `mtty-linux-x86_64.tar.gz`, `mtty-linux-x86_64.AppImage`, `dist/*.deb` | AppImage has an explicit `AppRun` entry point |
| Windows | `mtty-windows-x86_64.zip`, `mtty-app-<ver>-x86_64.msi` | MSI installation/uninstallation is exercised on the runner |

Every artifact gets a `.sig` next to it when signing is configured.
Detached signatures are generated once in the Linux assembly job, after platform
codesigning, and verified there against the committed public key.
All app bundles, archives and installers contain one native mtty application
and mtty-cli, plus `mtty-ptyhost`, the PTY host that keeps shells running
across restarts (ADR 0041). No separate native application archive is published.
All four runner builds and AppImage/MSI packaging are required to succeed.

## Secrets (all optional)

| Secret | Purpose |
|--------|---------|
| `APPLE_CERT_P12`, `APPLE_CERT_PASSWORD`, `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD` | codesign + notarize + staple the macOS build |
| `WINDOWS_CERT_PFX` (base64), `WINDOWS_CERT_PASSWORD` | `signtool` the MSI |
| `MINISIGN_SECRET_KEY` | sign every artifact with `minisign` (passwordless key) |

Without them the pipeline still produces usable (unsigned) artifacts and prints
what it skipped.

### Generating the minisign key

```sh
minisign -G -W -p minisign.pub -s minisign.key     # -W: no password, for CI
gh secret set MINISIGN_SECRET_KEY < minisign.key   # raw contents of the secret key
```

The repository keeps `minisign.pub` at its root, and the release job attaches it,
so `…/releases/download/<tag>/minisign.pub` is published with release artifacts.
The updater embeds the official public key; a custom channel can override it:

```toml
update-pubkey = "RWQ…"
```

## Backing up and rotating the minisign key

GitHub Secrets are **write-only**: once `MINISIGN_SECRET_KEY` is set you cannot
read it back from the UI or API. Losing the local copy does **not** break
releases (CI keeps signing), but you cannot recover the raw key either — so keep
an offline backup and treat the key as long-lived:

- The public key is in the repo (`minisign.pub`) and attached to every release.
- Back up the private key somewhere durable (password manager, encrypted copy,
  macOS Keychain). To rotate: generate a new pair, `gh secret set
  MINISIGN_SECRET_KEY`, replace `minisign.pub` and tell users to update
  `update-pubkey`. Old releases stay verifiable with the archived old public key.

## Update manifest

The release job writes `latest.json` and attaches it to the release:

```json
{
  "version": "0.1.0",
  "artifacts": {
    "macos-aarch64": {
      "url": "https://github.com/mttydev/mtty/releases/download/v0.1.0/mtty-macos-arm64.zip",
      "sha256": "…",
      "signature": "https://github.com/mttydev/mtty/releases/download/v0.1.0/mtty-macos-arm64.zip.sig"
    },
    "linux-x86_64": { "url": "…AppImage", "sha256": "…" },
    "windows-x86_64": { "url": "…msi", "sha256": "…" }
  }
}
```

Point the app at it:

```toml
update-check-url = "https://github.com/mttydev/mtty/releases/latest/download/latest.json"
```

Then *Settings → Check for Updates* reports the version, *Download Update* fetches
the artifact for the running platform, verifies its SHA-256 and required minisign
signature using the built-in public key (or `update-pubkey` override), and on macOS *Install and Relaunch*
swaps the bundle with a rollback helper (ADR 0025).

Platform keys match the app's `platform_key()`: `macos-aarch64`,
`macos-x86_64`, `linux-x86_64`, `linux-aarch64`, `windows-x86_64`.
`linux-x86_64-deb` is a manifest-only key for the `.deb` artifact (the app never
asks for it), and `release.yml` does not currently emit a `linux-aarch64`
manifest entry.

`scripts/build-update-manifest.py` requires both macOS architectures, Linux and
Windows plus the `.deb` entry, computes SHA-256 from the actual files, and prefers
AppImage/MSI over archive fallbacks. When minisign is configured, missing signatures fail the
assembly job; all detached signatures are verified against `minisign.pub`.
Before signing, `--collect-only` gathers nested `dist/` and `target/wix/` uploads
into the release directory and rejects duplicate filenames rather than overwriting
one package with another.
Run `python3 scripts/test-release-manifest.py` to check selection and failure cases.

## MSI (Windows) — verified

The WiX template is committed at [`mtty-app/wix/main.wxs`](../mtty-app/wix/main.wxs)
(it installs `mtty.exe` and `mtty-cli.exe` into
`%ProgramFiles%\mtty\bin`, adds that directory to the machine `PATH`, and
registers an uninstall entry). `cargo wix --package mtty-app` builds it; it
needs WiX 3.x (chocolatey `wixtoolset`), and must run **from `mtty-app/`**
because the template references `wix\License.rtf` relatively.

Verified end to end on a real Windows 11 host (2026-09-29): build → `msiexec /i`
→ both binaries on disk + PATH entry + "mtty 0.0.0" uninstall entry → the
installed app launches → `msiexec /x` removes the directory.


## Verifying installers

Both installers were checked on real hardware (2026-09-29).

**Windows MSI** — see the section above (build → install → PATH → run → uninstall).

**Linux `.deb` and AppImage** (Ubuntu 24.04):

```sh
cargo build --release -p mtty-app -p mtty-cli -p mtty-ptyhost
cargo install cargo-deb --locked && cargo deb -p mtty-app --no-build
sudo dpkg -i target/debian/mtty_*_amd64.deb     # /usr/bin/mtty{,-cli,-ptyhost}
mtty-cli ping                                   # runs; errors only because no host
```

The AppImage needs a **real 256x256 icon** — `appimagetool` refuses a 1x1
placeholder, and the desktop file must carry an `Icon=` key — so the pipeline
generates one with `scripts/make-icon.py` (standard library only). Result:
`mtty-linux-x86_64.AppImage` containing `mtty`, `mtty-cli` and `mtty-ptyhost`, and it
extracts cleanly (`--appimage-extract`).

**Not verified**: the GUI itself. The machine's X displays belong to the login
screen (no auth cookie), and under `xvfb` + software Vulkan (lavapipe) `wgpu`
fails with `Invalid surface` (a limitation of offscreen software Vulkan, not the
app). A real desktop session or a GPU is needed for that.

## Verifying a signed release (users)

Fetch the artifact, its `.sig`, and the published public key (`minisign.pub`):

```sh
curl -fsSLO https://github.com/mttydev/mtty/releases/download/v0.1.0/mtty-macos-arm64.zip
curl -fsSLO https://github.com/mttydev/mtty/releases/download/v0.1.0/mtty-macos-arm64.zip.sig
curl -fsSLO https://github.com/mttydev/mtty/releases/download/v0.1.0/minisign.pub
minisign -Vm mtty-macos-arm64.zip -p minisign.pub      # -> "Signature and comment signature verified"
```

The manifest ([`latest.json`](#update-manifest)) carries both a `sha256` and a
`signature` URL per artifact, so a downloader can check either or both:

```sh
shasum -a 256 mtty-macos-arm64.zip   # compare with the manifest's "sha256"
```

The updater requires a detached signature and the trusted public key. An unsigned
rehearsal artifact cannot be installed through the updater. Manual verification
of a downloaded package is separate from updater verification.

## Acceptance coverage and remaining work

Local single-application verification (2026-10-01): 154 workspace tests,
nine release/manifest tests, strict clippy, formatting and four enforced release
performance budgets passed. The single native `mtty.app` passed identity and
codesign checks. Both the packaged app and installed app completed the desktop
smoke, including native split-session migration and pane close/focus. This is
local macOS evidence. The single-application pipeline subsequently published
v0.0.4 successfully at `03848e4`:
[run 36794301755](https://github.com/mttydev/mtty/actions/runs/36794301755)
passed all four builds, AppImage/MSI packaging, package checks, assembly and
detached-signature verification. Each new candidate must repeat full CI and the
four-runner packaging rehearsal for its own commit.

The v0.0.6 changes (the mtty rename, agent workflows, hosts and SFTP/FTP,
verified updates, and the fixes found on real Linux and Windows desktops) are
in [the v0.0.6 release notes](releases/v0.0.6.md).

The v0.0.5 changes and resource limits are described in
[the release notes](releases/v0.0.5.md) and [the performance report](PERFORMANCE.md).
Local verification includes workspace tests, strict clippy, release performance
and allocation gates, the required-GPU shaping-cache test, and the macOS desktop
smoke. Remote CI/rehearsal and the final Release remain the authoritative records
for the published commit and package hashes; local results do not replace them.

The release workflow checks Linux package contents and CLI execution, and Windows
MSI install → two binaries + URL handler → CLI execution → uninstall. These
checks do not establish interactive desktop behavior. The real-host checks above
describe the earlier two-binary packages, not the new native-host payload.

Release rehearsal (2026-09-30, `16b2230`):
[run 36663466348](https://github.com/mttydev/mtty/actions/runs/36663466348)
passed all four build jobs and assembly. The downloaded `release-assembled`
artifact was independently checked: all five manifest entries matched their
SHA-256, all nine detached signatures verified, both macOS archives contained two
bundles with three binaries each and icons, and the Windows zip and Linux tar/deb
contained all three binaries. The manifest selects AppImage for Linux and MSI for
Windows. Windows install/URL-handler/uninstall checks passed on the runner. This
was a manual rehearsal; no GitHub Release was published.

Local macOS verification at the same commit: both downloaded bundles pass
`codesign --verify --deep --strict`; fmt, strict clippy, workspace tests and all
four release performance budgets pass. The binaries from the downloaded archive
were driven over MTP for Kitty inline graphics and Mermaid sequence/pie previews in both hosts,
with screenshots confirming the output. The eframe chrome now stays dark even
when the OS uses a light theme, keeping preview text legible.

Local close-out (2026-09-30, working-tree follow-up to `3357aab`): both hosts
use the same tab/session context-menu widget and group-boundary rule. Native
now draws the group dividers. Close Tab, tab `×` and session middle-click remove
the whole split tab; keyboard close retains focused-pane behavior. Regression
tests cover decoration persistence (including legacy defaults), menu visibility
and separator positions, focus after tab removal/movement, Close Below, and the
actual egui divider paint output. fmt, workspace clippy/tests, all four release
performance budgets, privacy scanning and seven manifest tests passed locally.

The macOS packaging script was also exercised with only its source/output roots
redirected to keep existing `dist/` artifacts untouched. Both newly built bundles
contained three binaries and the icon, passed `codesign --verify --deep --strict`
(ad-hoc signing), and ran CLI help without a host. This is **not** Developer ID
signing/notarization, GUI interaction acceptance, or a new four-runner rehearsal.
At this local verification stage, the edits had not yet been committed/pushed
or exercised by a new remote release workflow. The desktop acceptance items
below remain open regardless of subsequent packaging success.

The macOS update-helper test executes the swap and missing-download rollback
against temporary bundles with spaces in their paths, using a test relaunch
command. Archive selection rejects a native-only download and selects
`mtty.app` from a dual-bundle archive. These are component checks, not a real
download/install/relaunch acceptance. Windows zip updates also replace the native
binary along with the eframe host and CLI.

| Check | Acceptance criterion | Environment still needed |
|-------|----------------------|--------------------------|
| AppImage desktop launch | Execute the AppImage, open a pane, use MTP, then exit cleanly | Linux desktop |
| Wayland shortcut | Grant the portal request; invoke the shortcut with the app unfocused | Wayland desktop |
| Windows IME, mtty | Compose CJK text, check candidate position, commit/cancel, switch splits | Interactive Windows desktop |
| Update self-replace | Download/verify a newer version, install/relaunch, retain workspace; exercise failure recovery | Installed macOS app, Windows MSI/zip, Linux AppImage |
| Visual acceptance | Capture IME composition and Windows/Linux graphics/Mermaid previews; macOS graphics/previews verified above | Desktop sessions |
| Platform signing | Verify Developer ID/notarization and MSI Authenticode on shipped artifacts | Apple/Windows signing credentials |

Record the commit, package hash, commands, result and screenshots for each desktop
check. A manual rehearsal does not create live update-download URLs; a published
release (or a dedicated test manifest server) is needed for download acceptance.
