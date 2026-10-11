# Linux prebuilt warned or failed with GLIBC_2.39

## Symptoms

- Running an installed `subswap` prints:
  `subswap: /lib/x86_64-linux-gnu/libc.so.6: weak version 'GLIBC_2.39' not found (required by subswap)`
- Or the binary refuses to start on Ubuntu 22.04 / Debian 12-class hosts while GitHub Release assets were built on a newer runner.

## Cause

A plain `cargo build` on `ubuntu-latest` (24.04, glibc 2.39) records `GLIBC_2.39` in the ELF version-dependency section. Older hosts only ship 2.35/2.36, so the dynamic loader warns or aborts even when the symbol was marked weak.

## Remedy

Release Linux assets with `cargo zigbuild --target <triple>.2.28` and an `objdump` guard that rejects any binary whose highest `GLIBC_*` symbol exceeds 2.28. Document the floor as glibc 2.28+ (Ubuntu 20.04+, Debian 10+, RHEL 8+). Local overwrite installs on a 2.35 host should come from a release built under that floor, or from a native `cargo build --release` on that host.

## Verification

- `readelf -V "$(command -v subswap)"` / `objdump -T` show no `GLIBC_` above 2.28 for Release assets.
- On Ubuntu 22.04 (glibc 2.35), installed `subswap --version` and the default entry produce no glibc loader message.
