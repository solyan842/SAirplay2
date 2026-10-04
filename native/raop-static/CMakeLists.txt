cmake_minimum_required(VERSION 3.20)
project(sairplay_raop_static LANGUAGES C CXX)

if(NOT WIN32)
  message(FATAL_ERROR "SAirplay2 in-process RAOP bridge currently targets Windows only")
endif()
if(NOT DEFINED MSA_AIRPLAY_CLI_ROOT)
  message(FATAL_ERROR "MSA_AIRPLAY_CLI_ROOT is required")
endif()
if(NOT DEFINED OPENSSL_INCLUDE_DIR)
  message(FATAL_ERROR "OPENSSL_INCLUDE_DIR is required")
endif()
if(NOT DEFINED PTHREAD_INCLUDE_DIR)
  message(FATAL_ERROR "PTHREAD_INCLUDE_DIR is required")
endif()

set(MSA_ROOT "${MSA_AIRPLAY_CLI_ROOT}")
set(LIBRAOP "${MSA_ROOT}/libraop")
set(CROSSTOOLS "${LIBRAOP}/crosstools/src")
set(DMAP "${LIBRAOP}/dmap-parser")
set(ALAC_CODEC "${LIBRAOP}/libcodecs/alac/codec")
set(ALAC_ADDONS "${LIBRAOP}/libcodecs/addons")

foreach(required
    "${LIBRAOP}/src/raop_client.c"
    "${LIBRAOP}/src/rtsp_client.c"
    "${CROSSTOOLS}/platform.c"
    "${ALAC_ADDONS}/alac_wrapper.cpp"
    "${ALAC_CODEC}/ALACEncoder.cpp")
  if(NOT EXISTS "${required}")
    message(FATAL_ERROR "Pinned MSA/libraop source missing: ${required}")
  endif()
endforeach()

file(GLOB ALAC_C_SOURCES CONFIGURE_DEPENDS "${ALAC_CODEC}/*.c")
file(GLOB ALAC_CPP_SOURCES CONFIGURE_DEPENDS "${ALAC_CODEC}/*.cpp")

add_library(sairplay_raop_bridge STATIC
  "${CMAKE_CURRENT_SOURCE_DIR}/raop_bridge.cpp"
  "${LIBRAOP}/src/raop_client.c"
  "${LIBRAOP}/src/rtsp_client.c"
  "${LIBRAOP}/src/aes.c"
  "${LIBRAOP}/src/aes_ctr.c"
  "${LIBRAOP}/src/alac.c"
  "${DMAP}/dmap_parser.c"
  "${CROSSTOOLS}/cross_log.c"
  "${CROSSTOOLS}/cross_ssl.c"
  "${CROSSTOOLS}/cross_util.c"
  "${CROSSTOOLS}/cross_net.c"
  "${CROSSTOOLS}/platform.c"
  "${ALAC_ADDONS}/alac_wrapper.cpp"
  ${ALAC_C_SOURCES}
  ${ALAC_CPP_SOURCES}
)

set_target_properties(sairplay_raop_bridge PROPERTIES
  C_STANDARD 11
  C_STANDARD_REQUIRED YES
  CXX_STANDARD 17
  CXX_STANDARD_REQUIRED YES
  OUTPUT_NAME "sairplay_raop_bridge"
)

target_include_directories(sairplay_raop_bridge PRIVATE
  "${LIBRAOP}/src"
  "${CROSSTOOLS}"
  "${DMAP}"
  "${ALAC_ADDONS}"
  "${ALAC_CODEC}"
  "${OPENSSL_INCLUDE_DIR}"
  "${PTHREAD_INCLUDE_DIR}"
)

target_compile_definitions(sairplay_raop_bridge PRIVATE
  NDEBUG
  NOMINMAX
  WIN32
  _CRT_SECURE_NO_WARNINGS
  _WINSOCK_DEPRECATED_NO_WARNINGS
  _CRT_NONSTDC_NO_DEPRECATE
  OPENSSL_SUPPRESS_DEPRECATED
  SSL_STATIC_LIB
  __PTW32_STATIC_LIB
  CLEANUP=__PTW32_CLEANUP_C
)

if(MSVC)
  target_compile_options(sairplay_raop_bridge PRIVATE
    /wd4018 /wd4068 /wd4090 /wd4101 /wd4102 /wd4142 /wd4244 /wd4267 /wd4996 /wd5105
  )
endif()
