# Cairn — root CMake project.
# Only the C++ telemetry kernel is built here; Rust is built by cargo, Python is interpreted.
cmake_minimum_required(VERSION 3.24)

project(PCOptimizer
    VERSION 0.2.0
    DESCRIPTION "Cairn native telemetry kernel"
    LANGUAGES CXX)

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
