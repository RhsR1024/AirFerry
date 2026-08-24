# AirFerry Android native: ZXing-C++ QR decoder + JNI bridge.
#
# Fetches a SHA-256-pinned zxing-cpp source archive and builds a thin JNI wrapper
# that exposes a luminance-decode entry point to Kotlin.

cmake_minimum_required(VERSION 3.22.1)
project(airferry_zxing LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Disable C assertions (NDEBUG) globally — ZXing-C++ contains assert() calls
# (e.g. RegressionLine::intersect) that fire on partial/corrupted QR codes and
# call abort(), crashing the app. NDEBUG makes assert() a no-op.
add_compile_definitions(NDEBUG)

# Align ELF LOAD segments to 16 KiB (0x4000). Android 15+ devices may run with
# a 16 KiB page size (notably 2025 flagship devices on Android 16), and the
# kernel refuses to dlopen() a .so whose LOAD segments are only 4 KiB
# (0x1000) aligned — System.loadLibrary then throws UnsatisfiedLinkError and
# every QR decode silently fails, so the receiver "cannot scan anything".
# NDK r27's clang/ld supports 16 KiB; -z max-page-size=16384 forces the linker
# to round segment alignment up to 16 KiB so the same .so loads on both 4 KiB
# and 16 KiB page-size devices. This must be a target option (not a global
# CMAKE_EXE_LINKER_FLAGS) so it also reaches the link step of the ZXing static
# lib we link in.
add_link_options("-Wl,-z,max-page-size=16384")

# --- JNI bridge library ---
set(AIRFERRY_ZXING_CORE_DIR
    "${CMAKE_CURRENT_SOURCE_DIR}/../../../../../../core/zxing-decoder")
include("${AIRFERRY_ZXING_CORE_DIR}/zxing-source.cmake")
airferry_add_zxing()
add_library(airferry_zxing SHARED
    scan_jni.cpp
    "${AIRFERRY_ZXING_CORE_DIR}/airferry_zxing_core.cpp"
)
target_include_directories(airferry_zxing PRIVATE "${AIRFERRY_ZXING_CORE_DIR}")
target_link_libraries(airferry_zxing
    ZXing::ZXing
    log
    android
)
