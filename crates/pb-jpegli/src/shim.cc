// pb-jpegli's one entry point: a whole JPEG encode behind a C ABI, so no libjpeg struct crosses
// into Rust and no Rust panic unwinds through jpegli's C++ frames.
//
// Errors: libjpeg's contract is that `error_exit` never returns. jpegli's default prints and
// calls exit(); ours throws, and this file's catch turns it into an error code and message.
// That is safe because build.rs compiles jpegli with C++ exceptions on (libjxl's own build turns
// them off), so the unwind runs jpegli's destructors on the way out.
//
// Output: a destination manager over a std::vector, so a failed allocation is std::bad_alloc
// (caught here) rather than jpegli's mem dest writing through an unchecked malloc. On success
// the bytes go to the caller's `sink` once; nothing allocated here outlives the call.

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <exception>
#include <new>
#include <vector>

#include "lib/jpegli/encode.h"

namespace {

struct JpegliFailure {};

[[noreturn]] void ThrowOnError(j_common_ptr) { throw JpegliFailure{}; }

void Silent(j_common_ptr) {}

constexpr size_t kChunk = 64 * 1024;

struct VectorDestination {
  jpeg_destination_mgr pub;  // first, so a j_compress_ptr's dest casts back to this
  std::vector<uint8_t> bytes;
  size_t used = 0;  // bytes before the chunk jpegli is currently filling

  static void Init(j_compress_ptr cinfo) {
    auto* d = reinterpret_cast<VectorDestination*>(cinfo->dest);
    d->bytes.resize(kChunk);
    d->used = 0;
    d->pub.next_output_byte = d->bytes.data();
    d->pub.free_in_buffer = d->bytes.size();
  }

  // Called with the whole buffer full (libjpeg semantics); grows and hands back the new tail.
  static boolean Grow(j_compress_ptr cinfo) {
    auto* d = reinterpret_cast<VectorDestination*>(cinfo->dest);
    d->used = d->bytes.size();
    d->bytes.resize(d->bytes.size() * 2);
    d->pub.next_output_byte = d->bytes.data() + d->used;
    d->pub.free_in_buffer = d->bytes.size() - d->used;
    return TRUE;
  }

  static void Term(j_compress_ptr cinfo) {
    auto* d = reinterpret_cast<VectorDestination*>(cinfo->dest);
    d->bytes.resize(d->bytes.size() - d->pub.free_in_buffer);
  }
};

void CopyMessage(j_common_ptr cinfo, char* msg, size_t cap) {
  if (msg == nullptr || cap == 0) return;
  char buffer[JMSG_LENGTH_MAX] = {0};
  (*cinfo->err->format_message)(cinfo, buffer);
  size_t n = strnlen(buffer, sizeof buffer);
  if (n >= cap) n = cap - 1;
  memcpy(msg, buffer, n);
  msg[n] = '\0';
}

void CopyText(const char* text, char* msg, size_t cap) {
  if (msg == nullptr || cap == 0) return;
  size_t n = strlen(text);
  if (n >= cap) n = cap - 1;
  memcpy(msg, text, n);
  msg[n] = '\0';
}

}  // namespace

extern "C" {

// Encode `height` rows of `width` 8-bit pixels (`components` = 1 gray or 3 RGB, rows packed,
// `stride` bytes apart) at libjpeg `quality` 1-100: 4:4:4 and adaptive quantization (jpegli's
// defaults), progressive level 2. `app1` (the whole APP1 payload, `Exif\0\0` included) and `icc`
// are written when non-empty. Returns 0 and calls `sink(ctx, bytes, len)` once on success;
// otherwise returns non-zero with a message in `msg`.
int pb_jpegli_encode(const uint8_t* pixels, uint32_t width, uint32_t height,
                     uint32_t components, size_t stride, int quality,
                     const uint8_t* app1, size_t app1_len, const uint8_t* icc,
                     size_t icc_len,
                     void (*sink)(void* ctx, const uint8_t* bytes, size_t len),
                     void* ctx, char* msg, size_t msg_cap) {
  if (pixels == nullptr || sink == nullptr || width == 0 || height == 0 ||
      (components != 1 && components != 3) ||
      stride < static_cast<size_t>(width) * components ||
      app1_len > 65533) {
    CopyText("invalid arguments", msg, msg_cap);
    return 1;
  }
  jpeg_compress_struct cinfo;
  jpeg_error_mgr err;
  memset(&cinfo, 0, sizeof cinfo);
  cinfo.err = jpegli_std_error(&err);
  err.error_exit = ThrowOnError;
  err.output_message = Silent;
  VectorDestination dest;
  memset(&dest.pub, 0, sizeof dest.pub);
  dest.pub.init_destination = VectorDestination::Init;
  dest.pub.empty_output_buffer = VectorDestination::Grow;
  dest.pub.term_destination = VectorDestination::Term;
  bool created = false;
  int status = 0;
  try {
    jpegli_create_compress(&cinfo);
    created = true;
    cinfo.dest = &dest.pub;
    cinfo.image_width = width;
    cinfo.image_height = height;
    cinfo.input_components = static_cast<int>(components);
    cinfo.in_color_space = components == 1 ? JCS_GRAYSCALE : JCS_RGB;
    jpegli_set_defaults(&cinfo);
    jpegli_set_quality(&cinfo, quality < 1 ? 1 : (quality > 100 ? 100 : quality),
                       FALSE);
    // The library default is level 0 (sequential, standard Huffman tables); cjpegli and the
    // trial (export-plan.md, rung 4) use 2, which is where the measured savings come from.
    jpegli_set_progressive_level(&cinfo, 2);
    jpegli_start_compress(&cinfo, TRUE);
    if (app1_len > 0) {
      jpegli_write_marker(&cinfo, JPEG_APP0 + 1, app1,
                          static_cast<unsigned int>(app1_len));
    }
    if (icc_len > 0) {
      jpegli_write_icc_profile(&cinfo, icc, static_cast<unsigned int>(icc_len));
    }
    while (cinfo.next_scanline < cinfo.image_height) {
      JSAMPROW row = const_cast<JSAMPROW>(pixels + cinfo.next_scanline * stride);
      jpegli_write_scanlines(&cinfo, &row, 1);
    }
    jpegli_finish_compress(&cinfo);
  } catch (const JpegliFailure&) {
    CopyMessage(reinterpret_cast<j_common_ptr>(&cinfo), msg, msg_cap);
    status = 2;
  } catch (const std::bad_alloc&) {
    CopyText("out of memory", msg, msg_cap);
    status = 3;
  } catch (...) {
    CopyText("unexpected failure", msg, msg_cap);
    status = 4;
  }
  if (created) {
    try {
      jpegli_destroy_compress(&cinfo);
    } catch (...) {
      // Teardown frees jpegli's pools; nothing it reports changes the outcome.
    }
  }
  if (status == 0) sink(ctx, dest.bytes.data(), dest.bytes.size());
  return status;
}

}  // extern "C"
