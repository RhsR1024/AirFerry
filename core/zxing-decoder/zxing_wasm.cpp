// AirFerry WASM decoder: ZXing-C++ compiled to WebAssembly (M3 fast path).
//
// Exposes a luminance (Y-plane) multi-code decode entry point to the browser's
// decode worker, mirroring core/zxing-decoder/airferry_zxing_core.cpp (which is
// pure C++ and used by Windows). Feeding the Y plane directly (1 byte/pixel)
// skips the RGBA→grey conversion zxing-wasm performs internally and cuts the
// per-frame data by 4×. The packed wire layout is identical to PackMultiResults,
// so the web worker parses the same format as Windows/Android:
//   [u32 count LE][u32 payload_len LE][payload][4*s32 bbox LE]...
//
// Compiled with Emscripten as a STANDALONE_WASM module; JS drives it by calling
// the exported `malloc`/`free` and these functions (see CMakeLists.txt / build).
//
// ABI version lets the web worker detect this module and fall back to
// zxing-wasm when the self-built module is missing/failed to load.

#include "airferry_zxing_core.h"

#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>

extern "C" {

// Decode all QR codes in a full-frame Y (luminance) plane and return a malloc'd
// buffer in PackMultiResults wire layout, or nullptr when nothing was decoded.
// `out_len` receives the buffer size. Free with airferry_wasm_free().
__attribute__((used)) uint8_t* airferry_wasm_decode_multi_y(
    const uint8_t* pixels,
    size_t pixel_len,
    int32_t width,
    int32_t height,
    int32_t row_stride,
    size_t* out_len)
{
    if (out_len == nullptr) {
        return nullptr;
    }
    *out_len = 0;
    try {
        const auto results =
            AirFerryZxing::DecodeMultiFull(pixels, pixel_len, width, height, row_stride);
        const std::vector<uint8_t> packed = AirFerryZxing::PackMultiResults(results);
        if (packed.empty()) {
            return nullptr;
        }
        auto* buffer = static_cast<uint8_t*>(std::malloc(packed.size()));
        if (buffer == nullptr) {
            return nullptr;
        }
        std::memcpy(buffer, packed.data(), packed.size());
        *out_len = packed.size();
        return buffer;
    } catch (...) {
        return nullptr;
    }
}

// Tracked-region hot path (the web mirror of Android/Windows
// QrDecodePool::decodeMultiYTracked): decode only the expanded windows around
// the caller's last-known per-code bboxes instead of scanning the full frame.
// `hints` is a packed array of `hint_count` × 4 int32 {minX,minY,maxX,maxY} in
// full-frame pixel coords; `margin_fraction` expands each window (0.35 like
// the native tracker). Same packed wire layout as above — returned bboxes are
// in FULL-frame coords, ready to feed the tracker's nearest-slot update.
__attribute__((used)) uint8_t* airferry_wasm_decode_regions_y(
    const uint8_t* pixels,
    size_t pixel_len,
    int32_t width,
    int32_t height,
    int32_t row_stride,
    const int32_t* hints,
    size_t hint_count,
    float margin_fraction,
    size_t* out_len)
{
    if (out_len == nullptr) {
        return nullptr;
    }
    *out_len = 0;
    try {
        const auto results = AirFerryZxing::DecodeMultiRegions(
            pixels, pixel_len, width, height, row_stride,
            hints, hint_count, margin_fraction);
        const std::vector<uint8_t> packed = AirFerryZxing::PackMultiResults(results);
        if (packed.empty()) {
            return nullptr;
        }
        auto* buffer = static_cast<uint8_t*>(std::malloc(packed.size()));
        if (buffer == nullptr) {
            return nullptr;
        }
        std::memcpy(buffer, packed.data(), packed.size());
        *out_len = packed.size();
        return buffer;
    } catch (...) {
        return nullptr;
    }
}

__attribute__((used)) void airferry_wasm_free(void* ptr)
{
    std::free(ptr);
}

__attribute__((used)) uint32_t airferry_wasm_abi_version()
{
    return 1;
}

}  // extern "C"
