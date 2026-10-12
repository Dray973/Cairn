# Code signing policy

**Status:** Cairn is not code-signed yet. Each release names the SHA-256 of its setup in its
release notes, from 0.3.0 on also in `SHA256SUMS.txt`. A release that the release workflow of
this repository (`.github/workflows/release.yml`) builds on GitHub Actions also comes with a
build-provenance attestation, and its release notes show the command that checks it (see
[Verify a download](#verify-a-download)); 0.2.0 was built before that workflow existed and has
none. Cairn intends to sign its releases through the SignPath Foundation's program for
open-source projects; once the project is accepted, every release is signed under this policy.

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org).

## What Windows shows

A signed Cairn names **SignPath Foundation** as its publisher: in the UAC prompt when Cairn or
its setup asks for administrator rights, in the properties of its files and in Microsoft
Defender SmartScreen. SmartScreen can still warn about a new release until enough people have
downloaded it.

## How a release is signed

- Only the release workflow of this repository (`.github/workflows/release.yml`) can request a
  release signature. It runs on GitHub-hosted runners for a version tag on `main`, after the
  checks of `.github/workflows/ci.yml` passed for that commit. A build made anywhere else is
  never release-signed.
- Every request for a release signature is approved by hand by an approver, who checks that it
  comes from the tagged release commit and that its checks passed.
- A run of the release workflow started by hand on `main` rehearses the signing with SignPath's
  test certificate, which Windows does not trust; its files are never attached to a release.
- A release is signed in two rounds: first Cairn's own programs and libraries and the
  uninstaller, then the setup that is built around them.
- The GitHub and SignPath accounts of the team use multi-factor authentication.
- A signed release that turns out to be compromised is withdrawn and reported to SignPath.

## Team roles

- Authors: [Dray973](https://github.com/Dray973)
- Reviewers: [Dray973](https://github.com/Dray973)
- Approvers: [Dray973](https://github.com/Dray973)

## Signed files

Signed with the SignPath Foundation certificate, each carrying the product name Cairn and its
version in its version information:

- `Cairn.exe`, `optctl.exe` and `cairn-maintenance.exe`;
- `optimizer_engine.pyd`, the engine, and `optimizer_telemetry.dll`, the hardware monitor of the
  Dashboard, which reads the PC's performance counters and sends nothing anywhere;
- the uninstaller `unins000.exe` and the setup `Cairn-<version>-setup.exe`.

Bundled third-party files keep their publishers' signatures or have none, and are never signed
with the SignPath Foundation certificate:

- the Python 3.12 runtime and Tcl/Tk: signed by the Python Software Foundation;
- the Visual C++ runtime (`vcruntime140.dll`, `vcruntime140_1.dll`): signed by Microsoft. The
  hardware monitor links the Visual C++ runtime statically;
- Pillow's two extension modules (`_imaging`, `_imagingtk`): not signed by their authors. When
  Windows blocks either of them, Cairn still starts and shows built-in images instead of the
  ones it draws.

## Privacy

This program will not transfer any information to other networked systems unless specifically requested by the user or the person installing or operating it.

[privacy.md](privacy.md) lists when Cairn uses the network.

## Verify a download

```powershell
Get-FileHash .\Cairn-<version>-setup.exe -Algorithm SHA256        # compare with SHA256SUMS.txt
Get-AuthenticodeSignature .\Cairn-<version>-setup.exe | Format-List Status, SignerCertificate, TimeStamperCertificate
gh attestation verify .\Cairn-<version>-setup.exe --repo Dray973/Cairn
```

`gh attestation verify` checks that GitHub Actions built the setup from this repository; only a
release built by the release workflow has an attestation to check, and its release notes show
the command. Until the releases are signed, `Get-AuthenticodeSignature` reports them as not
signed.
