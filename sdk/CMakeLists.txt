cmake_minimum_required(VERSION 3.10)
project(Xgc2Module VERSION 0.3.0 LANGUAGES NONE)

# Header-only SDK: include/xgc2/module.h (the module ABI), include/xgc2/frame.h (the byte frame convention) and the
# C++ module SDK in sdk/include/xgc2/sdk/ (host.hpp, json_config.hpp, frame.hpp and the in-test host test_host.hpp).
# Consumers either install the package (find_package(Xgc2Module CONFIG REQUIRED)) or add this
# directory from their own project (add_subdirectory(<xgc2-module>/sdk)). Both give the
# imported INTERFACE target Xgc2Module::SDK; there is no library to link.
set(sdk_package_dir "share/cmake/Xgc2Module")

add_library(xgc2_module_sdk INTERFACE)
add_library(Xgc2Module::SDK ALIAS xgc2_module_sdk)
set_target_properties(xgc2_module_sdk PROPERTIES EXPORT_NAME SDK)
target_include_directories(xgc2_module_sdk INTERFACE
  $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/../include>
  $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
  $<INSTALL_INTERFACE:include>)

# The linker version script every module uses: it exports xgc2_module_entry and nothing else. Consumers read its
# path from Xgc2Module_EXPORTS_MAP (set by the package file, or here when the SDK is added as a subdirectory).
get_directory_property(sdk_has_parent PARENT_DIRECTORY)
if(sdk_has_parent)
  set(Xgc2Module_EXPORTS_MAP "${CMAKE_CURRENT_SOURCE_DIR}/exports.map" PARENT_SCOPE)
endif()
install(FILES exports.map DESTINATION "${sdk_package_dir}")
install(FILES ../include/xgc2/module.h ../include/xgc2/frame.h DESTINATION include/xgc2)
install(DIRECTORY include/xgc2/sdk DESTINATION include/xgc2)
install(TARGETS xgc2_module_sdk EXPORT Xgc2ModuleTargets)
install(EXPORT Xgc2ModuleTargets NAMESPACE Xgc2Module:: DESTINATION "${sdk_package_dir}")

# The version file is written by hand: write_basic_package_version_file(ARCH_INDEPENDENT)
# needs CMake 3.14 and Ubuntu 18.04 has 3.10.
configure_file(cmake/Xgc2ModuleConfig.cmake.in "${CMAKE_CURRENT_BINARY_DIR}/Xgc2ModuleConfig.cmake" @ONLY)
configure_file(cmake/Xgc2ModuleConfigVersion.cmake.in "${CMAKE_CURRENT_BINARY_DIR}/Xgc2ModuleConfigVersion.cmake" @ONLY)
install(FILES
  "${CMAKE_CURRENT_BINARY_DIR}/Xgc2ModuleConfig.cmake"
  "${CMAKE_CURRENT_BINARY_DIR}/Xgc2ModuleConfigVersion.cmake"
  DESTINATION "${sdk_package_dir}")
