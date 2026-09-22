cmake_minimum_required(VERSION 3.20)
project(sairplay_alac24 LANGUAGES C CXX)

if(NOT DEFINED ALAC_SOURCE)
  message(FATAL_ERROR "ALAC_SOURCE must point to the pinned Apple ALAC codec directory")
endif()

add_library(sairplay-alac24 SHARED
  alac24_wrapper.cpp
  "${ALAC_SOURCE}/EndianPortable.c"
  "${ALAC_SOURCE}/ALACBitUtilities.c"
  "${ALAC_SOURCE}/ALACEncoder.cpp"
  "${ALAC_SOURCE}/ag_enc.c"
  "${ALAC_SOURCE}/dp_enc.c"
  "${ALAC_SOURCE}/matrix_enc.c"
)

target_include_directories(sairplay-alac24 PRIVATE "${ALAC_SOURCE}")
target_compile_features(sairplay-alac24 PRIVATE cxx_std_17)
target_compile_definitions(sairplay-alac24 PRIVATE NOMINMAX _CRT_SECURE_NO_WARNINGS)

set_target_properties(sairplay-alac24 PROPERTIES
  OUTPUT_NAME "sairplay-alac24"
  PREFIX ""
)
