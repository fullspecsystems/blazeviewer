/*
 * dav1d C accessor shim (task #76 — animated AVIF on Windows).
 *
 * Compiled by pb-decode/build.rs (the `cc` crate) against the dav1d headers
 * installed next to the static lib we link (the vcpkg tree pinned by
 * scripts/setup-libheif.ps1 -VcpkgRef). That is the whole ABI strategy: dav1d's
 * public structs (Dav1dSettings, Dav1dData, Dav1dPicture, ...) are owned by
 * this file and resolved by the C compiler on every build — Rust sees only
 * opaque pointers plus the pb_* surface below, so a layout mismatch between
 * our code and the pinned dav1d is structurally impossible.
 *
 * Ownership contract (mirrored by the Rust RAII wrappers in src/dav1d.rs):
 *   - pb_dav1d_open        → pb_dav1d_close
 *   - pb_dav1d_data_new    → pb_dav1d_data_free   (safe after full consumption)
 *   - pb_dav1d_picture_new → pb_dav1d_picture_free (safe if never filled)
 */

#include <dav1d/dav1d.h>
#include <errno.h>
#include <stdlib.h>

/* ── Version ─────────────────────────────────────────────────────────────── */

/* The runtime library's version string, e.g. "1.5.3" (the vcpkg pin). */
const char *pb_dav1d_version(void) {
    return dav1d_version();
}

/*
 * Belt-and-braces runtime check, called before anything else: the linked lib's
 * API major must match the headers this shim was compiled against. Lib and
 * headers come from the same pinned vcpkg tree, so this can only fail on a
 * mixed/stale tree (e.g. a lib left behind from before a vcpkg ref change).
 */
int pb_dav1d_version_ok(void) {
    return DAV1D_API_MAJOR(dav1d_version_api()) == DAV1D_API_VERSION_MAJOR;
}

/* ── Decoder context ─────────────────────────────────────────────────────── */

/*
 * Open a decoder. n_threads / max_frame_delay <= 0 keep dav1d's defaults
 * (auto threads, auto delay); the animation backend passes small explicit
 * values (the decode pool already parallelizes across images — plan phase 5).
 * Grain synthesis stays at the default (applied): it is part of correct output.
 */
Dav1dContext *pb_dav1d_open(int n_threads, int max_frame_delay) {
    Dav1dSettings s;
    Dav1dContext *c = NULL;
    dav1d_default_settings(&s);
    if (n_threads > 0) s.n_threads = n_threads;
    if (max_frame_delay > 0) s.max_frame_delay = max_frame_delay;
    /* Silence dav1d's default stderr logger: a hostile/corrupt file surfaces
     * as a DecodeError upstream; console spam is noise (a NULL callback
     * disables logging). */
    s.logger.callback = NULL;
    s.logger.cookie = NULL;
    if (dav1d_open(&c, &s) < 0) return NULL;
    return c;
}

void pb_dav1d_close(Dav1dContext *c) {
    if (c) dav1d_close(&c);
}

/* ── Input data ──────────────────────────────────────────────────────────── */

/* Rust owns the sample buffer and guarantees it outlives the decode session
 * (the Data<'a> wrapper borrows the source bytes), so releasing a reference
 * frees nothing. */
static void pb_noop_free(const uint8_t *data, void *cookie) {
    (void)data;
    (void)cookie;
}

/*
 * Wrap one sample's bytes (a temporal unit of size-fielded OBUs, straight from
 * the avis mdat) without copying. `cookie` is the caller's sample index,
 * carried through dav1d as the timestamp and read back from the decoded
 * picture (pb_dav1d_picture_cookie) — the robust sample→picture mapping (a TU
 * can legally yield zero pictures, e.g. hidden alt-refs).
 */
Dav1dData *pb_dav1d_data_new(const uint8_t *ptr, size_t len, int64_t cookie) {
    Dav1dData *d = calloc(1, sizeof(*d));
    if (!d) return NULL;
    if (dav1d_data_wrap(d, ptr, len, pb_noop_free, NULL) < 0) {
        free(d);
        return NULL;
    }
    d->m.timestamp = cookie;
    return d;
}

/* Bytes dav1d has not consumed yet. 0 after a fully-consumed send; may shrink
 * without reaching 0 across EAGAIN (dav1d advances the buffer in place when a
 * packet holds several temporal units). */
size_t pb_dav1d_data_remaining(const Dav1dData *d) {
    return d->sz;
}

void pb_dav1d_data_free(Dav1dData *d) {
    if (!d) return;
    dav1d_data_unref(d); /* no-op on a fully-consumed (zeroed) data */
    free(d);
}

/*
 * Returns 0 (consumed), DAV1D_ERR(EAGAIN) (drain pictures, then re-send the
 * SAME data), or another negative error. Classify with pb_dav1d_err_is_again.
 */
int pb_dav1d_send(Dav1dContext *c, Dav1dData *d) {
    return dav1d_send_data(c, d);
}

int pb_dav1d_err_is_again(int rc) {
    return rc == DAV1D_ERR(EAGAIN);
}

/* ── Output pictures ─────────────────────────────────────────────────────── */

Dav1dPicture *pb_dav1d_picture_new(void) {
    return calloc(1, sizeof(Dav1dPicture));
}

/* 0 = picture filled; DAV1D_ERR(EAGAIN) = feed more data (or, after the last
 * sample, fully drained); other negative = decode error. */
int pb_dav1d_get_picture(Dav1dContext *c, Dav1dPicture *p) {
    return dav1d_get_picture(c, p);
}

void pb_dav1d_picture_free(Dav1dPicture *p) {
    if (!p) return;
    dav1d_picture_unref(p); /* no-op on a never-filled (zeroed) picture */
    free(p);
}

int pb_dav1d_picture_width(const Dav1dPicture *p) {
    return p->p.w;
}

int pb_dav1d_picture_height(const Dav1dPicture *p) {
    return p->p.h;
}

/* Dav1dPixelLayout: 0 = I400 (monochrome), 1 = I420, 2 = I422, 3 = I444. */
int pb_dav1d_picture_layout(const Dav1dPicture *p) {
    return (int)p->p.layout;
}

/* Bits per component: 8, 10 or 12. Planes hold u8 at 8, little-endian u16
 * above (strides are always in BYTES). */
int pb_dav1d_picture_bpc(const Dav1dPicture *p) {
    return p->p.bpc;
}

const uint8_t *pb_dav1d_picture_plane(const Dav1dPicture *p, int plane) {
    return (plane >= 0 && plane < 3) ? (const uint8_t *)p->data[plane] : NULL;
}

/* Byte stride for a plane index in 0..3 (dav1d stores one luma stride and one
 * shared chroma stride). */
ptrdiff_t pb_dav1d_picture_stride(const Dav1dPicture *p, int plane) {
    return p->stride[plane == 0 ? 0 : 1];
}

/* The sample-index cookie passed to pb_dav1d_data_new. */
int64_t pb_dav1d_picture_cookie(const Dav1dPicture *p) {
    return p->m.timestamp;
}

/* ── Sequence-header color metadata (CICP code points; -1 if absent) ─────── */
/* Used when the container carries no colr box (nclx colr overrides these per
 * MIAF — plan phase 6), and for the decode-time HDR backstop (transfer). */

int pb_dav1d_picture_matrix(const Dav1dPicture *p) {
    return p->seq_hdr ? (int)p->seq_hdr->mtrx : -1;
}

int pb_dav1d_picture_primaries(const Dav1dPicture *p) {
    return p->seq_hdr ? (int)p->seq_hdr->pri : -1;
}

int pb_dav1d_picture_transfer(const Dav1dPicture *p) {
    return p->seq_hdr ? (int)p->seq_hdr->trc : -1;
}

/* 1 = full range, 0 = limited (or unknown seq_hdr, which callers treat as
 * limited — the safe default for video-range content). */
int pb_dav1d_picture_full_range(const Dav1dPicture *p) {
    return p->seq_hdr ? p->seq_hdr->color_range : 0;
}

/* Dav1dChromaSamplePosition: 0 unknown, 1 vertical, 2 colocated. */
int pb_dav1d_picture_chroma_pos(const Dav1dPicture *p) {
    return p->seq_hdr ? (int)p->seq_hdr->chr : 0;
}
