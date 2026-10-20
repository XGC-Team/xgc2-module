cmake_minimum_required(VERSION 3.10)
project(Xgc2ModuleLinks VERSION 0.1.0 LANGUAGES CXX)

# The frame links of the platform, as modules of xgc2-module-host: peer_link (UDP between hosts) and shm_bridge
# (shared memory between the host processes of one machine). They are built against the installed SDK
# (libxgc2-module-dev: <xgc2/module.h>, <xgc2/frame.h>, <xgc2/sdk/*.hpp>) or, inside this repository, against
# ../sdk.
set(CMAKE_CXX_STANDARD 14)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
if(NOT TARGET Xgc2Module::SDK)
  find_package(Xgc2Module 0.3 CONFIG QUIET)
endif()
if(NOT TARGET Xgc2Module::SDK)
  add_subdirectory(${CMAKE_CURRENT_SOURCE_DIR}/../sdk sdk)
endif()
find_package(PkgConfig REQUIRED)
pkg_check_modules(JSONCPP REQUIRED jsoncpp)
find_package(Threads REQUIRED)

function(xgc2_link_module target source)
  add_library(${target} MODULE ${source})
  set_target_properties(${target} PROPERTIES PREFIX "lib" CXX_VISIBILITY_PRESET hidden VISIBILITY_INLINES_HIDDEN ON)
  target_compile_options(${target} PRIVATE -Wall -Wextra -Werror)
  target_include_directories(${target} PRIVATE ${JSONCPP_INCLUDE_DIRS})
  target_link_libraries(${target} PRIVATE Xgc2Module::SDK ${JSONCPP_LIBRARIES} Threads::Threads rt)
  set_property(TARGET ${target} APPEND_STRING PROPERTY LINK_FLAGS
    " -Wl,--no-undefined -Wl,--version-script=${Xgc2Module_EXPORTS_MAP}")
  install(TARGETS ${target} LIBRARY DESTINATION lib/xgc2/modules)
endfunction()

# The frame class (xgc2.frame2k.v1 by default) is a build option: a plan larger than 2032 bytes needs the 8k link.
xgc2_link_module(xgc2_peer_link peer_link/peer_link.cpp)
xgc2_link_module(xgc2_peer_link_8k peer_link/peer_link.cpp)
target_compile_definitions(xgc2_peer_link_8k PRIVATE XGC2_LINK_FRAME_CLASS=8k)
xgc2_link_module(xgc2_shm_bridge shm_bridge/shm_bridge.cpp)
xgc2_link_module(xgc2_shm_bridge_8k shm_bridge/shm_bridge.cpp)
target_compile_definitions(xgc2_shm_bridge_8k PRIVATE XGC2_LINK_FRAME_CLASS=8k)
