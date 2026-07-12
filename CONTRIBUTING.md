# Contributing to botworkz/tools

## Coverage

Unit-test coverage is measured with [cargo-tarpaulin](https://github.com/xd009642/tarpaulin)
using its LLVM engine. The HTML report lands at `target/tarpaulin/tarpaulin-report.html`
after a run; the XML report (`cobertura.xml`) is also emitted for CI tooling.

### Running coverage locally

```
cargo install cargo-tarpaulin --locked
make coverage
# or directly:
cargo tarpaulin
```

Config lives in `tarpaulin.toml` at the repo root; bare `cargo tarpaulin` picks
it up automatically.

### Scope: unit tests only

**Coverage numbers reflect unit tests only.**  The following test flows are
executed by dedicated CI jobs and are *not* instrumented by tarpaulin:

- **qemu/guestfish acceptance tests** — the `botforge` matrix in
  `.github/workflows/ci.yml` (driven by `_botforge.yml`) boots real QEMU VMs
  and verifies compressed qcow2 images with guestfish.  These require KVM and
  cannot run inside a plain `cargo test` environment.
- **qemu-img decode oracle** — the `_crate.yml` step that runs
  `native_compress_qemu_img_check_*` tests with `BOTFORGE_REQUIRE_QEMU=1`
  inside a qemu-utils container.

When reading coverage output, lines gated on `qemu_img_available()` or
`guestfish_available()` will appear uncovered.  That is expected; those paths
are covered by the integration CI jobs above.

### Threshold / fail-under

A `fail-under` percentage floor is intentionally **not set** in this PR.
The initial report establishes a baseline; the threshold will be ratcheted
upward in later dedicated PRs as coverage is improved.

### Coverage exclusions

A file may only be added to `exclude-files` in `tarpaulin.toml` if it is
**genuinely untestable by unit tests**, with the integration harness that
*does* cover it named explicitly here.

Current exclusions: *none.*

If you need to add an exclusion, open a PR that:
1. Adds the glob to `tarpaulin.toml`.
2. Adds a bullet to this section naming the file, explaining why it cannot be
   unit-tested, and identifying the CI job that provides integration coverage.
