# Cairn — root CMake project.
# Only the C++ telemetry kernel is built here; Rust is built by cargo, Python is interpreted.
cmake_minimum_required(VERSION 3.24)

# The version is the Rust workspace's: the [workspace.package] line of Cargo.toml, the only line
# of that file with exactly this shape. The DLL's version resource and version string follow it,
# and a change to Cargo.toml configures the project again.
set(CAIRN_CARGO_TOML "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml")
file(STRINGS "${CAIRN_CARGO_TOML}" CAIRN_VERSION_LINE
    REGEX "^version = \"[0-9]+\\.[0-9]+\\.[0-9]+\"$" LIMIT_COUNT 1)
string(REGEX REPLACE "^version = \"([0-9]+\\.[0-9]+\\.[0-9]+)\"$" "\\1"
    CAIRN_VERSION "${CAIRN_VERSION_LINE}")
if(NOT CAIRN_VERSION MATCHES "^[0-9]+\\.[0-9]+\\.[0-9]+$")
    message(FATAL_ERROR "Cargo.toml has no [workspace.package] version line (version = \"x.y.z\").")
endif()
set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS "${CAIRN_CARGO_TOML}")

project(PCOptimizer
    VERSION ${CAIRN_VERSION}
    DESCRIPTION "Cairn native telemetry kernel"
    LANGUAGES CXX RC)

if(NOT WIN32)
    message(FATAL_ERROR "Cairn targets Windows 11 x64 only.")
endif()
if(NOT CMAKE_SIZEOF_VOID_P EQUAL 8)
    message(FATAL_ERROR "64-bit build required (use -A x64).")
endif()

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# Multi-config generators (VS) → build/cpp/bin/<Config>/optimizer_telemetry.dll
set(CMAKE_RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/bin)
set(CMAKE_LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/bin)
set(CMAKE_ARCHIVE_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/lib)

# Static CRT: the DLL is loaded by an arbitrary python.exe; don't depend on a redist being present.
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")

option(TEL_BUILD_TESTS "Build the telemetry smoke-test executable" ON)

add_subdirectory(telemetry)
