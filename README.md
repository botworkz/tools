# tools

`botworkz/tools` is a cargo workspace that builds the small CLIs used across the botworkz toolchain:

- **[`shasset/`](./shasset)** — generic, verified-asset downloader and registry manager. Maintains a `shasset.yaml` manifest of named assets (`http(s)://`, `github-release://`, `oci://`) and fetches + verifies them against pinned SHA-256 checksums. Published as `ghcr.io/botworkz/tools/shasset`. The `shasset/` directory also hosts `bin/update-deps`.
- **[`botforge/`](./botforge)** — build-time CLI for VM artifact workflows: `deps`, `iso`, `payload`, `pack`, `run`, `test`. Wraps the QEMU/KVM, Packer, and ISO toolchains and is distributed as a batteries-included image at `ghcr.io/botworkz/tools/botforge`.
- **[`viscous/`](./viscous)** — opinionated, agent-friendly directory template generator. Reads a directory containing `__template__.yaml` and renders it into a destination directory, with declarative `for_each` / `when` / per-step conflict semantics. Published as `ghcr.io/botworkz/tools/viscous`.

See each app's `README.md` for usage, schema, and container instructions.

## Repository layout

```
.
├── shasset/        # shasset crate, container image, bin/update-deps, lib/*.sh
├── botforge/       # botforge crate and container image
├── viscous/        # viscous crate, template fixtures, container image
├── Earthfile       # +shasset-image, +botforge-image, +viscous-image, +images
├── Cargo.toml      # workspace root (members: shasset, botforge, viscous)
├── VERSION         # release version consumed by .github/workflows/ci.yml
└── .github/workflows/
    ├── ci.yml      # build, test, lint, publish images + GitHub Release on VERSION pushes
    └── _crate.yml  # reusable per-crate Rust + Dockerfile + smoke-test job
```

## Building locally

All images are built with [EarthBuild](https://github.com/EarthBuild/earthbuild):

```sh
earthly +shasset-image      # → botwork/shasset:local
earthly +botforge-image     # → botwork/botforge:local
earthly +viscous-image      # → botwork/viscous:local
earthly +images             # all of the above
```

## Releasing

Pushing a clean (non-`*-dev`) value to `VERSION` on `main` triggers `ci.yml` to publish container images to GHCR under the version tag and create a matching `vX.Y.Z` GitHub Release with the per-crate binaries attached. After a release, the `bump` job rolls `VERSION` to the next `*-dev` value.
