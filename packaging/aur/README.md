# AUR packages

Three source packages, one per whisper backend. Each compiles Spoke from
source so the GPU build matches the host driver (this is why there is no
prebuilt CUDA binary — see [BUILD.md](../../BUILD.md)).

| Package | Backend | Extra deps |
|---|---|---|
| `spoke-cpu` | CPU only | — |
| `spoke-vulkan` | Vulkan (any GPU) | `vulkan-icd-loader`, build: `shaderc` |
| `spoke-cuda` | NVIDIA CUDA | `cuda` |

They `provides=('spoke')` and `conflicts` with each other — only one at a time.

## Releasing a new version

1. Push a `vX.Y` git tag (the `source=` URL points at `archive/refs/tags/vX.Y`).
2. In each `PKGBUILD`: bump `pkgver`, reset `pkgrel=1`, update `sha256sums`:
   ```sh
   cd packaging/aur/spoke-cpu && makepkg -g   # prints the new sha256
   ```
3. Regenerate `.SRCINFO` for each: `makepkg --printsrcinfo > .SRCINFO`.
4. Push each package dir to its AUR git remote
   (`ssh://aur@aur.archlinux.org/spoke-<backend>.git`).

`pkgver` and the tarball's top-level dir (`Spoke-$pkgver`) must match the tag,
so keep the git tag as `v$pkgver`.

## Notes

- `license=('custom')` — the repo has no `LICENSE` file yet. Add one and set a
  real SPDX id (e.g. `MIT`) before AUR submission; namcap will warn otherwise.
