#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <vector>

namespace AirFerryZxing {

inline constexpr size_t MaxTrackedCodes = 4;

using Bbox = std::array<int32_t, 4>;

struct DecodeResult {
    std::vector<uint8_t> payload;
    Bbox bbox;
};

bool ValidLuminanceGeometry(
    const uint8_t* pixels,
    size_t pixel_len,
    int32_t width,
    int32_t height,
    int32_t row_stride) noexcept;

std::optional<DecodeResult> DecodeOneFull(
    const uint8_t* pixels,
    size_t pixel_len,
    int32_t width,
    int32_t height,
    int32_t row_stride);

std::optional<DecodeResult> DecodeOneRegion(
    const uint8_t* pixels,
    size_t pixel_len,
    int32_t width,
    int32_t height,
    int32_t row_stride,
    int32_t x,
    int32_t y,
    int32_t side);

std::vector<DecodeResult> DecodeMultiFull(
    const uint8_t* pixels,
    size_t pixel_len,
    int32_t width,
    int32_t height,
    int32_t row_stride);

std::vector<DecodeResult> DecodeMultiRegions(
    const uint8_t* pixels,
    size_t pixel_len,
    int32_t width,
    int32_t height,
    int32_t row_stride,
    const int32_t* hints,
    size_t hint_count,
    float margin_fraction);

// Wire layout shared by JNI and the Windows C ABI:
// [u32 count LE][u32 payload_len LE][payload][4*s32 bbox LE]...
std::vector<uint8_t> PackMultiResults(const std::vector<DecodeResult>& results);

}  // namespace AirFerryZxing
