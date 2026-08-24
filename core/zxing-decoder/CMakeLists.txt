# AirFerry WASM decoder (M3 fast path): ZXing-C++ compiled to WebAssembly.
#
# Build with Emscripten:
#   source ~/emsdk/emsdk_env.sh
#   cd core/zxing-decoder
#   emcmake cmake -B build-wasm -DCMAKE_BUILD_TYPE=Release
#   cmake --build build-wasm
#   ./link-wasm.sh build-wasm   # links libs → airferry_zxing.js/.wasm
#
# zxing-cpp is fetched as a SHA-256-pinned commit archive. To reuse an existing
# source tree (e.g. the Android build cache) instead of re-downloading, pass
#   -DZXING_SRC_DIR=<path/to/zxing-src>

cmake_minimum_required(VERSION 3.22.1)
project(airferry_zxing_wasm LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# ZXing-C++ contains assert() calls that abort() on partial/corrupted QR frames;
# NDEBUG turns them into no-ops (same as the Android native target).
add_compile_definitions(NDEBUG)

# Emscripten disables exceptions by default; ZXing-C++ throws on some big/partial
# frames. Enabling them everywhere lets the wasm catch and skip gracefully
# instead of trapping (numeric JS exception on 1080p → "扫不出来").
add_compile_options(-fexceptions)

include("${CMAKE_CURRENT_LIST_DIR}/zxing-source.cmake")
airferry_add_zxing()

# The linker emits the standalone .js/.wasm from the static libs (see
# link-wasm.sh). Keep it a static lib so it can be linked with em++ directly.
add_library(airferry_zxing STATIC
    airferry_zxing_core.cpp
    zxing_wasm.cpp
)
target_link_libraries(airferry_zxing PRIVATE ZXing::ZXing)
# -fexceptions: ZXing-C++ throws on some big/partial frames; without it the
# wasm traps and JS sees a numeric exception. Match link-wasm.sh.
target_compile_options(airferry_zxing PRIVATE -msimd128 -O3 -fexceptions)
set_target_properties(airferry_zxing PROPERTIES
    OUTPUT_NAME "airferry_zxing"
)
