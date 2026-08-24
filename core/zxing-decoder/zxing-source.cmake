include_guard(GLOBAL)

# One dependency identity for Android, Windows and Web/WASM. GitHub's archive
# bytes are SHA-256 pinned; restored source trees must also prove that identity
# before CMake is allowed to compile them.
set(AIRFERRY_ZXING_COMMIT "8dd1cf5c4fd6fb6211bb96713db926ac6f2cf825")
set(AIRFERRY_ZXING_ARCHIVE_SHA256 "8d9f87adfc45cd83735f23b8f1302b3c700b78804f2e4a1eac9ff779dd8d5f86")

function(airferry_add_zxing)
    include(FetchContent)
    if(NOT DEFINED ZXING_SRC_DIR OR ZXING_SRC_DIR STREQUAL "")
        set(_zxing_src "${CMAKE_CURRENT_BINARY_DIR}/_deps/zxing-src")
    else()
        get_filename_component(_zxing_src "${ZXING_SRC_DIR}" ABSOLUTE)
    endif()
    set(_stamp "${_zxing_src}/.airferry-verified-source")
    set(_expected_stamp
        "commit=${AIRFERRY_ZXING_COMMIT}\narchive_sha256=${AIRFERRY_ZXING_ARCHIVE_SHA256}")

    set(ZXING_READERS ON CACHE BOOL "" FORCE)
    set(ZXING_WRITERS OFF CACHE STRING "" FORCE)
    set(ZXING_EXAMPLES OFF CACHE BOOL "" FORCE)
    set(ZXING_BLACKBOX_TESTS OFF CACHE BOOL "" FORCE)
    set(ZXING_UNIT_TESTS OFF CACHE BOOL "" FORCE)
    set(ZXING_C_API OFF CACHE BOOL "" FORCE)
    set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)

    if(EXISTS "${_zxing_src}/CMakeLists.txt")
        set(_verified FALSE)
        if(EXISTS "${_stamp}")
            file(READ "${_stamp}" _actual_stamp)
            string(STRIP "${_actual_stamp}" _actual_stamp)
            if(_actual_stamp STREQUAL _expected_stamp)
                set(_verified TRUE)
            endif()
        endif()

        # Migrate legacy FetchContent Git caches only after proving that this
        # is the dependency worktree itself (not AirFerry's parent repo), that
        # HEAD is the pinned commit, and that no tracked source was modified.
        if(NOT _verified AND EXISTS "${_zxing_src}/.git")
            find_package(Git QUIET)
            if(GIT_FOUND)
                execute_process(
                    COMMAND "${GIT_EXECUTABLE}" -C "${_zxing_src}" rev-parse --show-toplevel
                    OUTPUT_VARIABLE _git_top OUTPUT_STRIP_TRAILING_WHITESPACE
                    RESULT_VARIABLE _top_result)
                execute_process(
                    COMMAND "${GIT_EXECUTABLE}" -C "${_zxing_src}" rev-parse HEAD
                    OUTPUT_VARIABLE _git_head OUTPUT_STRIP_TRAILING_WHITESPACE
                    RESULT_VARIABLE _head_result)
                execute_process(
                    COMMAND "${GIT_EXECUTABLE}" -C "${_zxing_src}" diff --quiet HEAD --
                    RESULT_VARIABLE _diff_result)
                file(REAL_PATH "${_zxing_src}" _source_real)
                if(_top_result EQUAL 0 AND _head_result EQUAL 0 AND
                   _diff_result EQUAL 0 AND _git_top STREQUAL _source_real AND
                   _git_head STREQUAL AIRFERRY_ZXING_COMMIT)
                    set(_verified TRUE)
                    file(WRITE "${_stamp}" "${_expected_stamp}\n")
                endif()
            endif()
        endif()

        if(NOT _verified)
            message(FATAL_ERROR
                "Existing ZXing source is stale or unverified: ${_zxing_src}. "
                "Remove that exact cache directory and reconfigure so the "
                "SHA-256-pinned archive can be fetched.")
        endif()
        # Never hand a caller-owned source directory to FetchContent's populate
        # step: it may delete/recreate SOURCE_DIR when stamps change.
        add_subdirectory(
            "${_zxing_src}"
            "${CMAKE_CURRENT_BINARY_DIR}/_deps/zxing-build"
            EXCLUDE_FROM_ALL)
    else()
        FetchContent_Declare(
            zxing
            URL "https://github.com/zxing-cpp/zxing-cpp/archive/${AIRFERRY_ZXING_COMMIT}.tar.gz"
            URL_HASH "SHA256=${AIRFERRY_ZXING_ARCHIVE_SHA256}"
            SOURCE_DIR "${_zxing_src}"
        )
        FetchContent_MakeAvailable(zxing)
        # This marker is written only after FetchContent's URL_HASH gate
        # succeeds. It prevents a later cache restore from silently crossing a
        # dependency-pin change.
        file(WRITE "${_stamp}" "${_expected_stamp}\n")
    endif()

    set(ZXING_SRC_DIR "${_zxing_src}" PARENT_SCOPE)
endfunction()
