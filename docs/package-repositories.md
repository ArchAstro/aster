# Linux package repository operations

Aster publishes the exact Linux x86-64 artifacts from a tagged GitHub release
to two distribution channels:

- a signed APT repository on GitHub Pages;
- a signed Yum/DNF repository on GitHub Pages.

Arch Linux installs use the release `.pkg.tar.zst` with `pacman -U`. The
`aster-bin` AUR package is not published until `AUR_PUBLISH_ENABLED` is set.

The release workflow refuses to rebuild packages during publication. RPM
packages are signed before their clean-container test and GitHub release. The
repository jobs download those released files, generate signed indexes, install
from the generated repositories, and then deploy. The APT repository also
generates an `aster-archive-keyring` package so existing clients receive signing
key updates through normal upgrades.

## One-time GitHub setup

Create a repository-specific OpenPGP signing key. Store its ASCII-armored
private key in the `PACKAGE_SIGNING_KEY` repository secret. If the key has a
passphrase, store it in `PACKAGE_SIGNING_PASSPHRASE`.

Configure GitHub Pages to use GitHub Actions, then add these repository
variables:

| Variable | Value |
| --- | --- |
| `PACKAGE_REPOSITORIES_ENABLED` | `true` |
| `PACKAGE_REPOSITORY_URL` | `https://archastro.github.io/aster` |

Do not enable publication until Pages is publicly reachable. The repository
workflow regenerates its indexes from every native package attached to every
GitHub release, so older installable versions survive each deployment.

## AUR (not published)

The `aster-bin` package is not published until `AUR_PUBLISH_ENABLED` is set.
Do not treat the AUR as a live install path. The supported Arch install is the
package attached to the GitHub release:

```console
sudo pacman -U ./aster-VERSION-1-x86_64.pkg.tar.zst
```

### One-time AUR setup

Publication stays off until `AUR_PUBLISH_ENABLED` is `true`. To prepare that
later publish, create a dedicated SSH key pair for release automation. Add the
public key to the Arch Linux account that will maintain `aster-bin`, and store
the private key in the `AUR_SSH_PRIVATE_KEY` repository secret.

Set `AUR_PUBLISH_ENABLED` to `true` only when that account is ready. The next
tagged release will create or update `ssh://aur@aur.archlinux.org/aster-bin.git`.
The workflow pins the published AUR Ed25519 host key and commits only
`PKGBUILD`, `.SRCINFO`, and the recipe's 0BSD `LICENSE`.

## Key rotation

Rotate keys across two tagged releases:

1. Keep the old private key in `PACKAGE_SIGNING_KEY`. Add the new public key,
   along with any retained old public keys, to `PACKAGE_SIGNING_PUBLIC_KEYS`.
   Publish a release. Existing APT clients receive a new
   `aster-archive-keyring` package signed by the still-trusted old key.
2. After the documented overlap window and another release, switch
   `PACKAGE_SIGNING_KEY` to the new private key. Keep the old public key in
   `PACKAGE_SIGNING_PUBLIC_KEYS` while retained RPMs still use it.

Repository generation exports every imported public key and verifies every
retained RPM against that keyring. Do not change the active key set and rerun
the same tag: the keyring package version is the Aster release version.

Remove an old public key only after deleting or retiring every RPM release asset
signed by it. The repository test fails if a retained RPM cannot be verified by
the active keyring.
