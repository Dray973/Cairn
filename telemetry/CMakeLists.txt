# optimizer_telemetry.dll: low-level hardware telemetry kernel.

# Release builds for other PCs turn this off, so the DLL also loads on CPUs without AVX2.
option(TEL_AVX2 "Build with /arch:AVX2" ON)

# The version resource (product name, version and description of the DLL) carries the project
# version, which the root CMakeLists.txt reads from Cargo.toml.
configure_file(src/version.rc.in "${CMAKE_CURRENT_BINARY_DIR}/version.rc" @ONLY)

add_library(optimizer_telemetry SHARED
    src/telemetry.cpp
    src/cpu.cpp
    src/memory.cpp
    src/process.cpp
    src/sampler.h
    include/telemetry/telemetry.h
    include/telemetry/ntinternal.h
    "${CMAKE_CURRENT_BINARY_DIR}/version.rc"
)

target_include_directories(optimizer_telemetry
    PUBLIC  $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
    PRIVATE ${CMAKE_CURRENT_SOURCE_DIR}/src)

target_compile_definitions(optimizer_telemetry
    PRIVATE
        TEL_EXPORTS                 # switches TEL_API to __declspec(dllexport)
        WIN32_LEAN_AND_MEAN
        NOMINMAX
        UNICODE _UNICODE
        _WIN32_WINNT=0x0A00
        WINVER=0x0A00
        _CRT_SECURE_NO_WARNINGS
        "TEL_VERSION=\"${PROJECT_VERSION}\"")  # tel_version_string()

# Compiler options apply to the C++ sources only; the resource compiler takes none of them.
target_compile_options(optimizer_telemetry PRIVATE
    $<$<COMPILE_LANGUAGE:CXX>:/W4 /permissive- /Zc:__cplusplus /utf-8 /EHsc>
    $<$<AND:$<COMPILE_LANGUAGE:CXX>,$<CONFIG:Release>>:/O2 /Oi /Ot /GL /Gy /Gw /guard:cf>
    $<$<AND:$<COMPILE_LANGUAGE:CXX>,$<CONFIG:Debug>>:/Od /Zi /RTC1>)
if(TEL_AVX2)
    target_compile_options(optimizer_telemetry PRIVATE $<$<COMPILE_LANGUAGE:CXX>:/arch:AVX2>)
endif()

# /PDBALTPATH records only the PDB's file name in the DLL, not the build folder's path. The name
# comes from CMake: the Visual Studio generator escapes the linker's own %_PDB% token into a
# literal string.
target_link_options(optimizer_telemetry PRIVATE
    $<$<CONFIG:Release>:/LTCG /OPT:REF /OPT:ICF /DEBUG:FULL /guard:cf
        /PDBALTPATH:$<TARGET_PDB_FILE_NAME:optimizer_telemetry>>
    $<$<CONFIG:Debug>:/DEBUG:FULL>)

# NtQuerySystemInformation is resolved at runtime from ntdll. powrprof supplies
# CallNtPowerInformation (nominal clock per core) and pdh the Processor Information
# "% Processor Performance" counter that scales it to the effective clock.
target_link_libraries(optimizer_telemetry PRIVATE kernel32 advapi32 powrprof pdh)

set_target_properties(optimizer_telemetry PROPERTIES
    OUTPUT_NAME "optimizer_telemetry"
    PREFIX ""
    POSITION_INDEPENDENT_CODE ON)

if(TEL_BUILD_TESTS)
    add_executable(telemetry_smoke tests/smoke.cpp)
    target_link_libraries(telemetry_smoke PRIVATE optimizer_telemetry)
    target_compile_definitions(telemetry_smoke PRIVATE WIN32_LEAN_AND_MEAN NOMINMAX)
    target_compile_options(telemetry_smoke PRIVATE /W4 /permissive- /EHsc /utf-8)
    target_link_libraries(telemetry_smoke PRIVATE psapi)
endif()
