# Linux ARM64 Release Builds

Date: 2026-10-18. Audience: codexctl release maintainers.

Build `aarch64-unknown-linux-gnu` natively on
`namespace-profile-sawmills-runner-arm64` inside an `ubuntu:22.04` job container.
The container keeps the release on the Ubuntu 22.04 baseline regardless of the
Namespace runner's base OS. The workflow installs native build prerequisites
before the existing Rust toolchain action.

Ubuntu 22.04's updated `libc6` package uses glibc 2.35. The container supplies GCC
and the workflow installs Rust. Rust supports
`aarch64-unknown-linux-gnu` as a Tier 1 target with native host tools.
These sources support reusing the current Cargo build without a cross compiler.
[Ubuntu package](https://packages.ubuntu.com/jammy-updates/libc6),
[Rust platform support](https://doc.rust-lang.org/rustc/platform-support.html)

## Compatibility Checks

After building, check that `getconf GNU_LIBC_VERSION` reports `glibc 2.35`
in the ARM64 container. Inspect both executables with `objdump -T` and refuse
any required GLIBC symbol version above `GLIBC_2.35`.
Run `codexctl --version` and `codexctl-central --help`
before packaging. This checks that the actual binaries start on the selected baseline.
Keep the existing locked build command:

```sh
cargo build --release --locked --target aarch64-unknown-linux-gnu
```

The release workflow must still demonstrate a successful native build and startup.
Startup checks cover loading and argument parsing; they do not establish
compatibility across every Linux distribution or exercise account operations.
