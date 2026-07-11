/*
 * dav1d C accessor shim (task #76 — animated AVIF on Windows).
 *
 * Compiled by pb-decode/build.rs (the `cc` crate) against the dav1d headers
 * installed next to the static lib we link (the vcpkg tree pinned by
 * scripts/setup-libheif.ps1 -VcpkgRef). That is the whole ABI strategy: dav1d's
 * public structs (Dav1dSettings, Dav1dPicture, ...) are owned by this file and
 * resolved by the C compiler on every build — Rust sees only opaque pointers
 * plus the pb_* surface below, so a layout mismatch between our code and the
 * pinned dav1d is structurally impossible.
 *
 * Phase 2 scope: version surface only (proves link + include path end to end).
 * The decode surface (open/send/next_picture/accessors/close) lands with the
 * demuxer in plan phases 3-6.
 */

#include <dav1d/dav1d.h>

/* The runtime library's version string, e.g. "1.5.3" (the vcpkg pin). */
const char *pb_dav1d_version(void) {
    return dav1d_version();
}

/*
 * Belt-and-braces runtime check, called before any API that writes a
 * caller-owned struct: the linked lib's API major must match the headers this
 * shim was compiled against. Lib + headers come from the same pinned vcpkg
 * tree, so this can only fail on a mixed/stale tree (e.g. a lib rebuilt after
 * a vcpkg ref change without re-running the setup script).
 */
int pb_dav1d_version_ok(void) {
    return DAV1D_API_MAJOR(dav1d_version_api()) == DAV1D_API_VERSION_MAJOR;
}
