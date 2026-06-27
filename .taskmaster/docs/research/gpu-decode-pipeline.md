# PhotoBlaze — GPU-Accelerated Decode & the VRAM-Resident Pipeline

Research date: 2026-06-26. Target: NVIDIA RTX 5090 (Blackwell, 32 GB GDDR7), Windows 11 primary, 7680×3840 @ 120 Hz, Rust + (likely) wgpu. Future macOS/Apple-Silicon port desired.

Scope: how to decode still images on the GPU, keep the decoded pixels resident in VRAM, and get them onto screen with the least possible CPU work and latency. Every nontrivial claim is cited by a bracketed number that maps to the **Sources** list at the bottom.

---

## 1. Overview & the core architectural thesis

The headline insight that should drive PhotoBlaze's architecture:

**If you decode on the GPU, the decoded pixels are *already in VRAM*. There is no CPU→VRAM upload at all — you only need an in-VRAM "interop" step to let the renderer (wgpu/Vulkan/D3D12) sample the buffer the decoder wrote.** This eliminates the single most expensive step in a CPU-decode pipeline (the per-image PCIe upload of a fat RGBA frame) and is the entire reason to pursue GPU decode for a speed-obsessed viewer.

For perspective on what CPU decode costs you: a wgpu video-playback study measured `write_texture()` uploads at ~995 MB/s for 4K30 and ~1,990 MB/s for 4K60 RGBA — fine over PCIe but pure overhead that GPU decode removes entirely [23]. A single 7680×3840 RGBA8 frame is ~118 MB (see §6); uploading that from CPU every time you flip photos is the thing to avoid.

The flip side and the **single biggest risk for this project**: getting a GPU-decoded buffer into a *wgpu* texture without a CPU round-trip is not supported by any stable wgpu API today (§4). That constraint, more than anything else, shapes whether PhotoBlaze can use wgpu as-is or must drop to raw Vulkan (`ash`) / D3D12 (`windows-rs`) for the decode-target textures.

At 120 Hz you have 8.3 ms per refresh. If a photo is pre-decoded and resident, *displaying* it is just a fullscreen textured quad — sub-millisecond. So the real engineering problem is **prefetch throughput and keeping the working set resident**, not per-frame draw cost. 32 GB of VRAM is generous (§6) — the binding constraints are decode throughput and the decode→texture interop, not capacity.

---

## 2. NVIDIA still-image decode: nvJPEG, nvJPEG2000, nvImageCodec

### 2.1 nvJPEG

nvJPEG is NVIDIA's CUDA library for GPU JPEG decode/encode, with single and batched decode paths [1][4]. Key facts for PhotoBlaze:

- **Backends.** Three modes: `NVJPEG_BACKEND_HARDWARE` (dedicated NVJPG decode engine), `NVJPEG_BACKEND_GPU_HYBRID` / `NVJPEG_BACKEND_DEFAULT` (CUDA-SM-based decode), and CPU fallback [3]. The hardware engine is **independent of the CUDA SMs**, so HW decode and shader work can run concurrently [3].
- **CRITICAL caveat for the RTX 5090.** The dedicated 5-core hardware JPEG engine and the documented `NVJPEG_BACKEND_HARDWARE` path are a **datacenter/pro feature (A100, A30, H100)** — the "Leveraging the Hardware JPEG Decoder" guidance is A100-specific and consumer GeForce cards historically fall back ("Hardware Decoder not supported") to the GPU-hybrid path [3]. NVIDIA does not advertise an exposed NVJPG hardware-decode block on consumer Blackwell, and the consumer Blackwell whitepaper similarly omits the dedicated decompression block that the datacenter parts have [31]. **Action item: do not assume `BACKEND_HARDWARE` works on the 5090 — query at runtime and benchmark GPU-hybrid; treat HW JPEG as "verify, don't assume."** (Open Question #1.)
- **Formats.** Baseline + progressive JPEG, 8-bit, Huffman, up to 4 channels; chroma subsampling 4:4:4, 4:2:2, 4:2:0, 4:4:0, 4:1:1, 4:1:0 [3]. Output can be RGB/BGR/RGBI/BGRI/YUV/grayscale.
- **Batching.** Hardware/batched paths shine at large batch sizes (the A100 HW path kicks in for batch >50) [1]. For a viewer this matters for *prefetch bursts* (decode the next N photos in one batched call) rather than one-at-a-time.
- **Throughput.** NVIDIA cites 4–8× over Tesla V100 and "up to 20×" over CPU-only on the HW path [3]; published per-image numbers in the A100 guide are workload-specific and not a clean GB/s figure [3]. Treat vendor multipliers as directional and **benchmark on the actual 5090**.

### 2.2 nvJPEG2000

Separate library for JPEG 2000 (digital pathology, satellite, some pro/cinema/DICOM workflows) [9][10]:

- **Throughput:** 232 images/sec for 1920×1080 8-bit at batch size 20 on an RTX A6000; batching ~3× over single-image; >8× single-image latency vs a 16-thread CPU; ~24× with GPU+batching combined [9].
- **Tiling:** multi-stream tile decode (10 CUDA streams) cut total decode time ~75% vs single-stream on a GV100 [9].
- **Formats:** 8-bit and 12-bit, lossless (5-3 DWT) and lossy (9-7 DWT); 4:4:4 plus 4:2:0/4:2:2 (interleaved output of subsampled streams requires enabling RGB output) [9][10]. Pascal+ GPUs [10].
- **Relevance:** JPEG 2000 is rare in consumer photo libraries — low priority unless PhotoBlaze targets pathology/GIS/cinema DCP users.

### 2.3 nvImageCodec — the library you actually want to build on

`nvImageCodec` is NVIDIA's newer **unified, Apache-2.0** decode framework that wraps nvJPEG, nvJPEG2000 (incl. High-Throughput JPEG2000), and nvTIFF behind one interface, with CPU fallbacks via libjpeg-turbo/libtiff/OpenCV [6][7][8]. Why it's the right foundation:

- **Formats:** JPEG, JPEG2000, TIFF, BMP, PNG, PNM, WebP [6]. (Note: **no AVIF/HEIF** — those go through NVDEC, §3.)
- **Backends** selectable per-call: GPU hardware engine, GPU/CUDA hybrid, or CPU, with **codec prioritization + automatic fallback** [6] — exactly the "use HW if present, else GPU, else CPU" policy PhotoBlaze needs given the 5090 HW-JPEG uncertainty.
- **Zero-copy device interfaces** to CuPy/PyTorch/CV-CUDA, i.e. it decodes **directly into CUDA device memory** you control [6] — the prerequisite for VRAM-resident, no-round-trip operation.
- **Batched, variable-shape, heterogeneous** decode in one call [6] — ideal for prefetch bursts.
- License Apache-2.0 [6].

### 2.4 Rust integration

- **`nvjpeg-sys`** — raw FFI bindings to nvJPEG exist on crates.io/docs.rs [5]. Thin, `-sys` only; you write the safe wrapper.
- **`cudarc`** — actively maintained safe-ish CUDA host API wrapper (driver/runtime, memory, streams) [44][45]; the pragmatic choice for CUDA context/stream/memory management and FFI plumbing to nvImageCodec.
- **`cust` / Rust-CUDA** — the Rust-CUDA project was rebooted in 2025 [46]; `cust` provides host-side CUDA. Still maturing; `cudarc` is the safer bet today.
- **nvImageCodec has C/C++ and Python bindings but no Rust bindings** [6] — you will hand-roll `bindgen` FFI against its C API. This is the main integration cost, but the C API is clean and stable.
- All of nvJPEG/nvJPEG2000/nvImageCodec/NVDEC are **NVIDIA-proprietary, CUDA-only, NVIDIA-GPU-only** — none of this ports to Apple Silicon (§7).

---

## 3. NVDEC for still images: AVIF (AV1 intra) and HEIF/HEIC (HEVC intra)

**Feasibility: yes in principle, with real friction. There is no turnkey "GPU AVIF/HEIF decoder" — you assemble one.**

### 3.1 What NVDEC gives you

NVDEC is the on-chip video decode block; it decodes AV1, HEVC/H.265, H.264, VP9, VP8, MPEG-2, VC-1 [11][14], runs fully independently of the graphics/compute engines, and writes **NV12/P010 surfaces into VRAM** [11]. You retrieve frames as a **CUDA device pointer** via `cuvidMapVideoFrame()` (and `cuvidUnmapVideoFrame()`), which is the hook for CUDA→graphics interop [11]. There is an explicit **`ulIntraDecodeOnly` flag** to tell the driver the stream is I/IDR-only (still images) so it optimizes memory — documented for HEVC, H.264, VP9 [11].

On the **RTX 5090 (Blackwell)** specifically: **3 NVENC + 2 NVDEC** engines, 9th-gen NVENC, with AV1 (incl. "AV1 UHQ"), HEVC, H.264, and new 4:2:2 support; H.264 decode throughput is doubled vs prior gen [16][17][18]. Two independent NVDEC engines means you can run parallel decode sessions for prefetch.

### 3.2 The friction (this is the important part)

AVIF (AV1 in HEIF/ISOBMFF) and HEIC (HEVC in HEIF) are **container formats, not raw codec streams** [47][last search]. To use NVDEC you must, on the CPU:

1. **Parse ISOBMFF/HEIF** (boxes, `iref`/`dimg`, properties) to find the coded image items, alpha aux item, and grid layout (libheif/libavif do this) [47].
2. **Handle grid/tiled images.** HEIC and AVIF very commonly store one logical photo as an **N×M grid of independently-coded tiles** (e.g. 512×512) joined by a `dimg` reference [last search]. You must feed **each tile** to NVDEC and **stitch** the results into one surface — non-trivial bookkeeping, and many small decodes carry per-surface overhead.
3. **Feed the elementary stream** (AV1 OBUs / HEVC NAL units) to NVDEC via the cuvid parser.
4. **Decode alpha separately** (alpha is a second monochrome coded item) and **apply AV1 film-grain / monochrome / 10–12-bit** handling yourself.
5. **Convert NV12/P010 → RGBA** with a CUDA or compute shader (NVDEC never outputs RGBA) [11].

Additional caveats: NVDEC has **minimum dimension constraints** and per-session setup latency, so tiny thumbnails or very small tiles can be *slower* on NVDEC than on CPU; HEVC must be in the **Main / Main10 Still Picture** profile NVDEC supports; AVIF must be a profile/level NVDEC's AV1 decoder accepts. There is **no published "AVIF on NVDEC" reference implementation from NVIDIA** — you're integrating libheif/libavif (for parse) + NVDEC (for the codec) + a YUV→RGBA kernel yourself.

### 3.3 Practical recommendation

- **JPEG → nvImageCodec/nvJPEG.** This is the 90%+ case and the cleanest GPU path.
- **HEIC/AVIF → start with CPU decode (libheif + dav1d/libde265, or system Windows codecs), measure, and only build the NVDEC path if HEIC/AVIF is a hot path for your users.** The container parse + tile stitch + YUV→RGBA + alpha work is substantial; the payoff is real only for large single-tile images decoded in volume.
- For an MVP, **Windows Imaging Component (WIC)** / the OS HEIF codec can decode HEIC/AVIF on CPU with minimal code, deferring the NVDEC effort.

---

## 4. CUDA ↔ graphics interop & the wgpu question (the crux)

### 4.1 The mechanism (works, well-documented, API-agnostic)

CUDA's **external resource interop** is the standard, supported way to share a texture's backing memory between a graphics API and CUDA with no copy [24][25]:

- **Memory:** `cudaImportExternalMemory` / `cuImportExternalMemory` with handle types incl. `OpaqueWin32` (Vulkan/D3D shared handle), `D3D12Heap`, `D3D12Resource`, and `OpaqueFd` (Linux) [24]. CUDA then maps a device pointer / mipmapped array *aliasing the graphics resource's memory*.
- **Sync:** `cudaImportExternalSemaphore` with a D3D12 fence, Vulkan timeline/binary semaphore, or `OpaqueWin32` handle — so the decode (CUDA) and the draw (graphics queue) synchronize on the GPU with **no CPU stall** [24][25].

Canonical zero-copy flow for PhotoBlaze on Windows:

1. Graphics API allocates an **exportable** texture (D3D12 committed resource with shared flag, or Vulkan `VkImage`/`VkDeviceMemory` with external-memory handle).
2. Export an OS handle; `cudaImportExternalMemory` it into CUDA.
3. nvImageCodec/nvJPEG/NVDEC decode **into that aliased device memory** (for NVDEC, run the NV12→RGBA kernel writing into it).
4. Signal a shared fence/semaphore; the render queue waits on it; draws the texture. **Zero CPU round-trip, all in VRAM.**

This is proven technology (NVIDIA's CUDA samples ship Vulkan and D3D12 interop demos [25]). The hard part is not CUDA — it's the *graphics-side* allocation and **whether wgpu lets you do step 1 and wrap the result in step 4.**

### 4.2 The wgpu limitation — precise status (as of wgpu v29 stable, 2026-03 / v30 trunk)

**wgpu (the Rust crate) has NO stable, high-level API to import an externally-created/CUDA texture.** This is the documented, current reality [19][21][22][23]:

- The high-level `ExternalTexture` RFC (#3145) and "import external textures" requests (#2320, #965, #4067) are **open/unresolved** for native Rust; `ExternalTexture` landed only in the *browser/Dawn* WebGPU path and can only be constructed from JS objects (HTMLVideoElement/VideoFrame), **not from C/C++/Rust** [23][last search].
- The only native route is **`Device::create_texture_from_hal`**, explicitly an **internal, unstable HAL API with no stability guarantees, requiring separate per-backend code** (Vulkan/DX12/Metal) [23]. In v30 it gained a required `initial_state` param specifically to support zero-copy hardware-decoded video imports [22].
- **What the wgpu HAL *does* now expose (v30/trunk)** — useful but low-level [22]:
  - **Vulkan:** `vulkan::Device::texture_from_dmabuf_fd()` (**Linux/dmabuf only**) with `VULKAN_EXTERNAL_MEMORY_FD` / `VULKAN_EXTERNAL_MEMORY_DMA_BUF` feature flags; and `vulkan::Queue::add_wait_semaphore`/`remove_wait_semaphore` to wait on **external producers (CUDA/OpenCL/D3D12 via `VK_KHR_external_semaphore_*`)** without a CPU block.
  - **DX12:** `dx12::Texture::with_plane_slice` (wrap NV12 planes), `dx12::Queue::add_wait_fence`/`add_signal_fence` for cross-API fence sync.
  - **Metal:** `metal::Queue::add_wait_event`/`add_signal_event` (`MTLSharedEvent`) for foreign-API GPU sync.
- **There is NO wgpu equivalent of Dawn's `SharedTextureMemory`** (which cleanly imports DXGI shared handles, D3D11/D3D12 textures, IOSurface, AHardwareBuffer as sampleable WebGPU textures) [last search][26]. **Dawn — the C++ WebGPU implementation — has the zero-copy import API that wgpu lacks** [26].

### 4.3 What this means for PhotoBlaze — the decision

| Path | Zero-copy GPU decode? | Effort / risk |
|---|---|---|
| **Pure wgpu, CPU-side staging** | No — decode in VRAM then read back to CPU then `write_texture` (defeats the purpose) | Low effort, but throws away the main win |
| **wgpu + HAL `create_texture_from_hal`** (allocate exportable D3D12/Vulkan texture yourself via `ash`/`windows-rs`, import to CUDA, wrap back) | **Yes** | **High** — unstable HAL API may break each wgpu release; per-backend code; the documented "hard" path [23] |
| **Raw `ash` (Vulkan) or `windows-rs` (D3D12), no wgpu** for the decode/render core | **Yes**, cleanest interop | High up-front, but stable APIs; full control of external memory |
| **Dawn (C++ WebGPU) via FFI instead of wgpu** | **Yes** via `SharedTextureMemory` [26] | Medium — but means C++ dependency, not idiomatic Rust |

**Recommendation:** prototype on **wgpu with CPU staging first** (get the viewer working), but architect the texture-allocation layer behind a trait so the decode-target textures can be swapped to a **HAL/`ash`/D3D12 external-memory path** once zero-copy is the bottleneck. **Be aware up front that committing to wgpu means the zero-copy interop will ride on unstable HAL APIs (`create_texture_from_hal`) or a parallel raw-Vulkan/D3D12 allocator** — this is the central wgpu constraint to flag to the owner. (Open Question #2.)

---

## 5. Windows GPU upload / streaming paths (when you *do* touch the CPU→VRAM path)

Relevant whenever decode stays partly on CPU (HEIC/AVIF MVP, PNG, fallback) and you must move RGBA into VRAM with minimal latency.

### 5.1 D3D12 GPU Upload Heaps + ReBAR — lowest-latency CPU→VRAM for write-once data

`D3D12_HEAP_TYPE_GPU_UPLOAD` (Agility SDK 1.710.0, March 2023; retail-usable without Dev Mode since 1.613.0, March 2024) exposes **ReBAR memory: VRAM that the CPU can write directly via a persistent mapped pointer**, with CPU writes forwarded straight to VRAM over PCIe [32][33][34].

- **Requires Resizable BAR** enabled (entire VRAM CPU-visible); check `D3D12_FEATURE_DATA_D3D12_OPTIONS16::GPUUploadHeapSupported` [32][34]. The 5090 + a modern board will have ReBAR.
- **GPU reads are as fast as `HEAP_TYPE_DEFAULT`** (data lives in VRAM) — so you skip the usual UPLOAD-heap→DEFAULT-heap copy entirely [32][34].
- **Persistently map it** (map once, never Map/Unmap per use) [32][33]. NVIDIA measured moving a buffer to a GPU upload heap dropping a workload's GPU time from 0.2 ms to <0.01 ms (data served from VRAM not the sysmem aperture) [34].
- **Hard rules:** memory is **write-combined / uncached** — write **sequentially via `memcpy`**, **never read back** (CPU reads are "extremely slow"); strides >32 DWORDs can be 2× slower [32][34]. Counts against VRAM budget; needs double/triple-buffering to avoid overwriting in-flight data [32].
- **Verdict:** for a CPU-decoded RGBA frame, `memcpy` straight into a persistently-mapped GPU-upload-heap texture is the **lowest-latency CPU→VRAM path on Windows** — no staging copy, no copy-queue submit. This is the right fallback path. The wgpu/Vulkan analog is a `HOST_VISIBLE | DEVICE_LOCAL` (ReBAR) memory type, but wgpu does not expose heap-type selection, so this specific optimization is another reason the decode-target allocation may want to live below wgpu.

### 5.2 The D3D12 copy queue

The classic path (UPLOAD heap → `CopyTextureRegion` on the dedicated copy/DMA queue → DEFAULT heap) overlaps transfer with graphics/compute and is the safe general-purpose option, but adds a staging copy and queue sync vs. the GPU-upload-heap direct write. Use it for large bulk transfers where you want async DMA overlap; use GPU upload heaps for latency-sensitive write-once frames.

### 5.3 DirectStorage + GPU decompression — useful, but NOT a JPEG/HEIF decoder

Important scoping correction: **DirectStorage's GPU decompression decodes the GDeflate format, not JPEG/HEIF/AVIF** [27][28][29]. GDeflate is a DEFLATE-derivative reorganized for 32-way GPU parallelism, decompressed on the compute queue; a request is GPU-decompressed only when its destination is a D3D12 resource [27][28]. So DirectStorage helps PhotoBlaze in two specific ways, not as a photo codec:

1. **Fast NVMe→VRAM file I/O** (even with `DSTORAGE_COMPRESSION_FORMAT_NONE`) — bypasses slow Win32 file I/O to stream raw image bytes near drive speed; useful for the prefetcher reading thousands of files.
2. **A transcoded BCn cache:** pre-transcode the library to BC7 (§6), **GDeflate-compress those BC7 textures**, and stream them NVMe→VRAM with GPU decompression — very fast cold loads of a curated/processed library. (Not applicable to arbitrary first-time JPEG/HEIC viewing.)

Blackwell handles GPU decompression with **no measurable frame-rate penalty across the stack (5090→5060)**, and the 5090 does it better than the 4090 — though notably the *consumer* Blackwell whitepaper shows **no dedicated decompression block** (unlike datacenter Blackwell), so the gains come from raw throughput / 1.79 TB/s GDDR7 bandwidth [31]. DirectStorage is Windows-only (NuGet `Microsoft.Direct3D.DirectStorage`, 1.2/1.3 stable, 1.4 preview) [30][27] — **not portable to macOS** (§7).

### 5.4 Lowest-latency summary

- **GPU-decoded image:** no upload — CUDA-graphics interop in VRAM (§4). Best case, zero PCIe traffic for pixels.
- **CPU-decoded image, latency-critical:** persistent-mapped **GPU Upload Heap (ReBAR)** `memcpy` [32][34].
- **Bulk/cold library load:** **DirectStorage** (raw streaming, or GDeflate'd BCn cache) [27].

---

## 6. VRAM budgeting on 32 GB

### 6.1 The math (per full-canvas 7680×3840 = 29.49 Mpx frame)

| Format | Bytes/px | Per 7680×3840 frame | Per 24 MP (6000×4000) photo |
|---|---|---|---|
| RGBA8 (uncompressed) | 4 | **~118 MB** | ~96 MB |
| RGBA16 / RGBA16F (HDR, wide-gamut) | 8 | **~236 MB** | ~192 MB |
| **BC7** (compressed-in-VRAM) | 1 | **~29.5 MB** | ~24 MB |
| BC6H (HDR compressed) | 1 | ~29.5 MB | ~24 MB |

BC7 is 8 bpp = exactly **4:1 vs RGBA8, 8:1 vs RGBA16**, and remains **directly GPU-sampleable with zero decode cost at draw time** (fixed-function hardware) [36][37].

### 6.2 Residency capacity (assume ~27 GB usable after ~3–5 GB for swapchain, compositor, driver, and in-flight decode working set — a 7680×3840 triple-buffered RGBA16 HDR swapchain alone is ~0.7 GB)

| Resident format | Full-canvas frames in ~27 GB | Practical takeaway |
|---|---|---|
| RGBA8 | **~230** | Already a huge prefetch window |
| RGBA16 (HDR) | **~115** | Still large |
| BC7 | **~900** | Enormous — basically "keep everything you'd ever flick to" |

**Conclusion: 32 GB is not the binding constraint.** Even uncompressed RGBA8 holds ~230 screen-sized frames resident; a photo viewer only needs a prefetch window of tens of frames around the cursor. So budget is comfortable — spend the surplus on (a) higher prefetch depth, (b) keeping a full-res copy for zoom *and* a screen-fit mip, and/or (c) RGBA16 for HDR fidelity.

### 6.3 Should you keep decoded frames resident vs. re-decode?

**Keep them resident.** Re-decoding costs codec time + (if CPU) re-upload; a resident RGBA/BC7 texture costs only VRAM you have in abundance. Use an LRU window: prefetch ±N around the current index, evict beyond. With 32 GB you can make N large enough that the user essentially never out-runs the cache.

### 6.4 BC7/ASTC compression-in-VRAM tradeoffs

- **BC7** gives 4:1 with excellent quality (≈45 dB PSNR at 8 bpp, ~0.5 dB better than ASTC-8bpp on average) [36]. The cost is **encode time**: BC7 encoding is 10–50× slower than BC1 and is a CPU/compute step you'd run on first view (then cache) — *not* something to do in the hot flick path [36]. Decode is free (hardware) [36].
- **ASTC** offers flexible bitrates (8 bpp → <1 bpp) but **NVIDIA desktop GPUs do not natively sample ASTC** — it's a mobile/Apple format. On Windows/NVIDIA, **use BCn (BC7 for SDR color, BC6H for HDR)** [36][37].
- **When is in-VRAM compression worth it?** Only if you want a >230-frame resident window or are VRAM-constrained on *other* GPUs. Given 32 GB, the pragmatic default is **uncompressed RGBA8 (or RGBA16 for HDR) for the live window**, with **BC7 reserved for a large persistent cache / the GDeflate'd disk cache** (§5.3). Don't pay BC7 encode cost in the interactive path.

---

## 7. macOS / Apple-Silicon portability

None of the NVIDIA stack (CUDA, nvJPEG, nvImageCodec, NVDEC, DirectStorage) exists on Apple Silicon. The equivalents — and the **big architectural advantage** — are:

- **Unified Memory Architecture (UMA):** CPU and GPU share the same physical DRAM with no PCIe bus; a pointer the CPU writes, the GPU reads from the same memory [last searches]. With `MTLResourceStorageModeShared` buffers, **there is no CPU→VRAM upload at all** — the §5 upload problem simply vanishes on Mac. This makes Apple Silicon arguably the *easier* zero-copy target.
- **Hardware decode:**
  - **VideoToolbox** — HW HEVC/H.264/AV1(M3+)/ProRes decode; can do **0-copy decode→display** producing `CVPixelBuffer`s backed by **IOSurface** [41][42]. SDL added VideoToolbox 0-copy decode/display [42].
  - **ImageIO** — decodes HEIC/JPEG/PNG/etc.; the default iPhone HEVC-HEIC (often tiled) path [last search]. Apple's HEIF/HEVC stack is mature (WWDC 2017 #511) [43].
- **Interop:** decoded `CVPixelBuffer`/`IOSurface` → Metal texture via `CVMetalTextureCache` (zero-copy). **Dawn (and thus a Dawn-backed WebGPU) imports IOSurface via `SharedTextureMemory`** [26] — but **wgpu (Rust) again lacks the high-level import**, so the Metal HAL (`metal::Queue::add_wait_event` etc. [22]) or raw `metal-rs` would be the path, mirroring the Windows situation.
- **Texture formats — good news for portability:** Apple Silicon Macs running **macOS support BCn (incl. BC7) *and* ASTC** [38][39][40] (Aras's M1 testing and Apple's feature-set tables confirm M1+ macOS supports BC, ASTC, ETC2, even PVRTC) [38]. So **a BC7-based VRAM/disk cache is portable to macOS** — *but only on macOS*; **iOS/iPadOS do NOT support BCn** (ASTC only) [38][40]. If an iOS port is ever in scope, the cache must be ASTC there.

### A portable hardware-decode abstraction

Define a backend trait roughly:

```
trait GpuImageDecoder {
    // Decode a compressed image (or container item) and return a handle to a
    // GPU-resident surface in the renderer's native texture type. Never returns CPU pixels.
    fn decode_to_resident(&self, bytes: &[u8], fmt: CodecHint) -> ResidentTexture;
}
```

- **Windows impl:** nvImageCodec (JPEG/JPEG2000/PNG/TIFF/WebP) + NVDEC (HEIC/AVIF) → CUDA external-memory interop into a D3D12/Vulkan texture.
- **macOS impl:** ImageIO / VideoToolbox → `CVPixelBuffer`/IOSurface → `CVMetalTextureCache` → Metal texture.
- The **renderer-texture type** is the abstraction seam. Because **wgpu can't represent "externally-owned texture" portably**, the seam likely sits **below wgpu** (raw D3D12/Vulkan on Windows, Metal on macOS) — which is the strongest argument for either (a) not using wgpu for the decode-target textures, or (b) accepting the unstable HAL path on both platforms. Keep all CPU-staging fallbacks behind the same trait so the viewer works everywhere before the zero-copy paths are built.

---

## 8. Recommendations (ranked, opinionated)

1. **Build the decode layer on `nvImageCodec` (not bare nvJPEG) for everything it covers (JPEG/JPEG2000/PNG/TIFF/WebP), via hand-rolled `bindgen` FFI + `cudarc` for CUDA context/stream/memory.** Use its codec-prioritization/auto-fallback so you transparently get HW-JPEG *if* the 5090 exposes it, GPU-hybrid otherwise, CPU as last resort [6]. Apache-2.0, batched, decodes straight into device memory [6].
2. **Treat zero-copy CUDA→texture interop as the core differentiator, and design the texture-allocation layer to live below (or beside) wgpu from day one.** The proven path is CUDA external memory + external semaphore importing a D3D12/Vulkan exportable texture [24][25]. Expect to use wgpu's unstable `create_texture_from_hal` or raw `ash`/`windows-rs`. **Do not assume stock wgpu can do this — it cannot today** [19][23].
3. **Ship an MVP on plain wgpu with CPU staging first** (libjpeg-turbo / WIC decode → persistent-mapped GPU-upload-heap or `write_texture`). Validate UX, prefetch, and the 120 Hz flick feel before investing in interop. Architect the texture source behind a trait (§7) so the GPU-decode backend slots in later.
4. **JPEG-first; defer HEIC/AVIF GPU decode.** Do HEIC/AVIF on CPU (libheif/WIC) in the MVP; only build the NVDEC path (container parse + tile stitch + NV12→RGBA + alpha, §3.2) if telemetry shows HEIC/AVIF is a hot path. There is no off-the-shelf GPU AVIF decoder — it's real integration work.
5. **Keep decoded frames resident; don't re-decode.** Use a large LRU prefetch window in RGBA8 (or RGBA16 for HDR). 32 GB easily holds 100–230 screen-sized frames uncompressed (§6) — capacity is not your constraint; decode throughput and interop are.
6. **Reserve BC7 for the *persistent/cold* cache, not the hot path.** Optionally pair BC7 + DirectStorage GDeflate for blazing cold loads of a processed library [27][36]. BC7 is portable to macOS (not iOS) [38]. Don't pay BC7 encode cost during interactive flicking.
7. **Lowest-latency CPU→VRAM = persistent-mapped D3D12 GPU Upload Heap (ReBAR), sequential `memcpy`, never read back** [32][34]. This is the fallback-path upload primitive; another reason the texture allocator may want to be below wgpu (which doesn't expose heap types).
8. **For the Mac port, lean into UMA** — `MTLResourceStorageModeShared` removes the upload problem; ImageIO/VideoToolbox → IOSurface → `CVMetalTextureCache`. Same "import external texture" wgpu gap applies, so plan for Metal HAL / `metal-rs`.

---

## 9. Performance / tradeoffs table

| Path | Where decode runs | CPU→VRAM cost | Latency | Portable to macOS? | Effort | Notes |
|---|---|---|---|---|---|---|
| nvImageCodec/nvJPEG + CUDA interop | GPU (CUDA/HW-JPEG) | **None** (stays in VRAM) | **Lowest** | No (NVIDIA-only) | High (interop) | The target architecture [6][24] |
| NVDEC (HEIC/AVIF) + interop | GPU video engine | None | Low (after parse) | No | High (parse+stitch) | 2 NVDEC on 5090; container work on CPU [11][16] |
| CPU decode → GPU Upload Heap (ReBAR) | CPU | ~118 MB/frame over PCIe5, persistent-map memcpy | Low | n/a (Win path) | Low | Best CPU-side upload [32][34] |
| CPU decode → wgpu `write_texture` | CPU | ~118 MB/frame, staging | Medium | Yes (portable) | **Lowest** | MVP default; portable but wastes the GPU-decode win [23] |
| BC7 transcode + DirectStorage GDeflate | GPU (GDeflate) | Streamed NVMe→VRAM | Low (cold load) | No (DStorage Win-only) | Medium | Only for processed/cached library [27][36] |
| macOS VideoToolbox/ImageIO + UMA | GPU (Apple) | **None** (unified memory) | Lowest | Yes (Mac-only) | Medium | IOSurface→CVMetalTextureCache [41][42] |

---

## 10. Open questions for the project owner

1. **Does the RTX 5090 expose a usable hardware JPEG decode engine (`NVJPEG_BACKEND_HARDWARE` / NVJPG) to nvJPEG/nvImageCodec, or only GPU-hybrid?** Documented HW-JPEG is datacenter-only (A100/H100); consumer Blackwell is unconfirmed [3][31]. **This needs an empirical benchmark on the actual hardware** and materially affects JPEG throughput ceilings.
2. **wgpu vs. raw graphics API for the decode-target textures.** Zero-copy GPU decode is impossible through stock wgpu's public API today [19][23]. Decide: (a) accept the unstable HAL `create_texture_from_hal` path, (b) write a parallel `ash`/`windows-rs` + `metal-rs` allocator below wgpu, or (c) consider Dawn (C++ WebGPU, has `SharedTextureMemory`) [26]. This is the most consequential architecture decision.
3. **Is HEIC/AVIF a first-class requirement or a nice-to-have?** It dictates whether to invest in the substantial NVDEC container/tile/alpha integration (§3.2) or rely on CPU/WIC decode.
4. **HDR scope?** RGBA16/BC6H + a wide-gamut/HDR swapchain doubles per-frame VRAM and changes the color pipeline. 32 GB still accommodates it (~115 full-canvas RGBA16 frames), but it's a deliberate choice.
5. **iOS/iPadOS ever in scope?** If yes, the in-VRAM compression format must be **ASTC** there (no BCn on iOS), unlike macOS which supports BC7 [38][40].
6. **Source-resolution zoom vs. screen-fit only?** Holding both full-res (for pixel-peeping zoom) and a screen-fit mip roughly doubles per-image VRAM — still fine at 32 GB, but affects the residency math and prefetch depth.

---

## 11. Sources

1. nvJPEG — CUDA Toolkit Documentation (latest 13.x). https://docs.nvidia.com/cuda/nvjpeg/index.html
2. nvJPEG Release 12.4 (PDF, Mar 2024). https://docs.nvidia.com/cuda/archive/12.4.0/pdf/nvJPEG.pdf
3. "Leveraging the Hardware JPEG Decoder and NVIDIA nvJPEG Library on A100 GPUs" — NVIDIA Technical Blog. https://developer.nvidia.com/blog/leveraging-hardware-jpeg-decoder-and-nvjpeg-on-a100/
4. nvJPEG — NVIDIA Developer. https://developer.nvidia.com/nvjpeg
5. nvjpeg-sys (Rust FFI bindings) — docs.rs. https://docs.rs/nvjpeg-sys/latest/nvjpeg_sys/
6. NVIDIA/nvImageCodec — GitHub (formats, backends, zero-copy, Apache-2.0). https://github.com/NVIDIA/nvImageCodec
7. nvImageCodec — NVIDIA Developer. https://developer.nvidia.com/nvimagecodec
8. "Advancing Medical Image Decoding with GPU-Accelerated nvImageCodec" — NVIDIA. https://developer.nvidia.com/blog/advancing-medical-image-decoding-with-gpu-accelerated-nvimagecodec/
9. "Accelerating JPEG 2000 Decoding … nvJPEG2000" — NVIDIA Technical Blog. https://developer.nvidia.com/blog/accelerating-jpeg-2000-decoding-for-digital-pathology-and-satellite-images-using-the-nvjpeg2000-library/
10. nvJPEG2000 API Reference / User Guide. https://docs.nvidia.com/cuda/nvjpeg2000/userguide.html
11. NVDEC Video Decoder API Programming Guide (SDK 13.0). https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvdec-video-decoder-api-prog-guide/index.html
12. NVDEC Application Note (SDK 13.0). https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvdec-application-note/index.html
13. Video Codec SDK — NVIDIA Developer. https://developer.nvidia.com/video-codec-sdk
14. NVDEC — Wikipedia. https://en.wikipedia.org/wiki/Nvidia_NVDEC
15. "Improving Video Quality and Performance with AV1 and Ada Lovelace" — NVIDIA. https://developer.nvidia.com/blog/improving-video-quality-and-performance-with-av1-and-nvidia-ada-lovelace-architecture/
16. "RTX 5090 Video Encoding — First Look" (NVENC/NVDEC counts) — Code Calamity. https://codecalamity.com/nvidia-rtx-5090-video-encoding-first-look/
17. NVIDIA GeForce RTX 5090 FE Review — Architecture — TechPowerUp. https://www.techpowerup.com/review/nvidia-geforce-rtx-5090-founders-edition/2.html
18. NVIDIA RTX Blackwell GPU Architecture (whitepaper PDF). https://images.nvidia.com/aem-dam/Solutions/geforce/blackwell/nvidia-rtx-blackwell-gpu-architecture.pdf
19. wgpu #2320 — Texture memory import API. https://github.com/gfx-rs/wgpu/issues/2320
20. wgpu #3145 — RFC: Introduce `ExternalTexture`. https://github.com/gfx-rs/wgpu/issues/3145
21. wgpu #965 — Interop with underlying graphics API. https://github.com/gfx-rs/wgpu/issues/965
22. wgpu CHANGELOG (v29/v30 interop entries: dmabuf, add_wait_semaphore, with_plane_slice, add_wait_fence, create_texture_from_hal). https://github.com/gfx-rs/wgpu/blob/trunk/CHANGELOG.md
23. "CPU→GPU Transfer Cost When Playing Video with wgpu, and the Road to Zero-Copy" (2026-03-04) — ginokent. https://ginokent.github.io/en/posts/2026-03-04-wgpu-video-playback-pipeline/
24. CUDA Driver API — External Resource Interoperability. https://docs.nvidia.com/cuda/cuda-driver-api/group__CUDA__EXTRES__INTEROP.html
25. CUDA Programming Guide — Graphics Interop / CUDA Interoperability with APIs. https://docs.nvidia.com/cuda/cuda-programming-guide/04-special-topics/graphics-interop.html
26. "Zero-Copy GPU Compute on Camera Frames in React Native" (Dawn `SharedTextureMemory`: DXGI/D3D11/IOSurface). https://dev.to/kbrandwijk/zero-copy-gpu-compute-on-camera-frames-in-react-native-what-actually-worked-512j
27. "DirectStorage 1.1 Now Available" — DirectX Developer Blog. https://devblogs.microsoft.com/directx/directstorage-1-1-now-available/
28. microsoft/DirectStorage — GDeflate README. https://github.com/microsoft/DirectStorage/blob/main/GDeflate/README.md
29. "Accelerating Load Times … GDeflate for DirectStorage" — NVIDIA. https://developer.nvidia.com/blog/accelerating-load-times-for-directx-games-and-apps-with-gdeflate-for-directstorage/
30. Microsoft.Direct3D.DirectStorage — NuGet. https://www.nuget.org/packages/Microsoft.Direct3D.DirectStorage/
31. "Testing DirectStorage with GPU decompression — do Blackwell GPUs have the upper hand?" — Tom's Hardware. https://www.tomshardware.com/pc-components/gpus/testing-directstorage-with-gpu-decompression-do-blackwell-gpus-have-the-upper-hand
32. "Effective Use of the New D3D12_HEAP_TYPE_GPU_UPLOAD" — AMD GPUOpen. https://gpuopen.com/learn/using-d3d12-heap-type-gpu-upload/
33. D3D12 GPU Upload Heaps — DirectX-Specs. https://microsoft.github.io/DirectX-Specs/d3d/D3D12GPUUploadHeaps.html
34. "Optimizing DX12 Resource Uploads Using GPU Upload Heaps" — NVIDIA Technical Blog. https://developer.nvidia.com/blog/optimizing-dx12-resource-uploads-to-the-gpu-using-gpu-upload-heaps/
35. "GPU Memory Pools in D3D12" — TheRealMJP. https://therealmjp.github.io/posts/gpu-memory-pool/
36. "Texture Compression Formats Explained: DXT, ASTC, BCn" — Texturize. https://texturize.app/blog/texture-compression-explained
37. ARM astc-encoder — Format Overview. https://github.com/ARM-software/astc-encoder/blob/main/Docs/FormatOverview.md
38. "Texture Compression on Apple M1" — Aras Pranckevičius. https://aras-p.info/blog/2021/01/18/Texture-Compression-on-Apple-M1/
39. Metal Feature Set Tables (PDF) — Apple. https://developer.apple.com/metal/Metal-Feature-Set-Tables.pdf
40. "Compressed Texture Formats in Metal" — Metal by Example. https://metalbyexample.com/compressed-textures/
41. VideoToolbox — Apple Developer Documentation. https://developer.apple.com/documentation/videotoolbox
42. "SDL: 0-copy decode and display using Apple VideoToolbox". https://discourse.libsdl.org/t/sdl-added-support-for-0-copy-decode-and-display-using-apple-videotoolbox/46500
43. WWDC 2017 Session 511 — Working with HEIF and HEVC. https://asciiwwdc.com/2017/sessions/511
44. cudarc — docs.rs. https://docs.rs/cudarc
45. coreylowman/cudarc — GitHub. https://github.com/coreylowman/cudarc
46. "Rebooting the Rust CUDA project" — Rust GPU. https://rust-gpu.github.io/blog/2025/01/27/rust-cuda-reboot/
47. strukturag/libheif (HEIF/AVIF parse + grid handling). https://github.com/strukturag/libheif
