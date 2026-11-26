# pb-jpegli vendored sources

Everything here is third-party, copied unmodified apart from `libjpeg-turbo/jconfig.h`, which is
written by hand (it is a CMake template upstream). `build.rs` compiles the `.cc` files with the
`cc` crate; nothing else in this folder is built.

| Folder | What | From | Licence |
|---|---|---|---|
| `libjxl/` | jpegli's encoder (`lib/jpegli`, minus the decoder, tests and libjpeg wrapper), the six `lib/jxl/base` headers and `lib/include/jxl/types.h` it includes | [libjxl](https://github.com/libjxl/libjxl) tag `v0.10.2` (`e1489592a770b989303b0edc5cc1dc447bbe0515`) | BSD-3-Clause (`LICENSE`, plus `PATENTS`) |
| `libjpeg-turbo/` | the libjpeg API headers jpegli implements: `jpeglib.h`, `jmorecfg.h`, `jerror.h` | [libjpeg-turbo](https://github.com/libjpeg-turbo/libjpeg-turbo) `8ecba3647edb6dd940463fedf38ca33a8e2a73d1` (2.1.5.1), the submodule commit libjxl v0.10.2 pins | IJG + BSD-3-Clause (`LICENSE.md`, `README.ijg`) |
| `highway/` | Highway's headers (`hwy/**/*.h`, minus tests, contrib and examples) | [Highway](https://github.com/google/highway) `457c891775a7397bdb0376bb1031e6e027af1c48` (v1.2.0), as jxl-sys 0.1.12+libjxl-0.12.0 vendors it | Apache-2.0 or BSD-3-Clause (`LICENSE`, `LICENSE-BSD3`) |

## Why Highway is here but never built

jpegli calls Highway at run time (CPU-target dispatch, aligned allocation). Those calls resolve
against the static libhwy that **jxl-sys** builds for JPEG XL export, so the app carries one
Highway. The headers here are that same release, so jpegli is compiled against exactly the
library it links. The jpegli-sys crate instead built a second libjxl with Highway 1.1.0, and its
libhwy won the link for libjxl 0.12 too: on Arm chips with SVE, the target bits differ between
1.1 and 1.2 (task #157).

## Refreshing

The Highway headers must always match jxl-sys's Highway: `pb-jpegli/Cargo.toml` pins jxl-sys with
`=`, and `build.rs` fails when `hwy/base.h` isn't the version it expects. To move either:

1. Bump jxl-sys (in `pb-encode` and here), then find the Highway commit its libjxl pins
   (`THIRD_PARTY_HIGHWAY` in `libjxl/deps.sh` inside the jxl-sys crate).
2. Run `refresh.sh <libjxl checkout> <libjpeg-turbo checkout> <highway checkout>` with each
   checked out at the commit you want (libjpeg-turbo at the commit that libjxl's
   `third_party/libjpeg-turbo` submodule pins).
3. Update `HWY_VERSION` in `build.rs`, the table above and `THIRD-PARTY-NOTICES.md`, then run
   `cargo test -p pb-jpegli` and the `pb-encode` JPEG tests.

Moving jpegli to the standalone [google/jpegli](https://github.com/google/jpegli) repository is
a possible later step; re-measure with `codec_bench` before changing what ships.
