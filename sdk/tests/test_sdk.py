#!/usr/bin/env python3
"""Check the header-only SDK the way a module author consumes it.

A consumer project includes <xgc2/module.h> from C11 and C++11 and compiles with all warnings
as errors. It is built three ways: from the source tree (add_subdirectory), against an
installed prefix (find_package) and against a copy of that prefix moved elsewhere. Refused
cases are checked too: a wrong major version, a missing header and a missing package file.

    sdk/tests/test_sdk.py --work-dir DIR [--prefix DIR]

With --prefix the installed package at that prefix is checked and nothing is built from the
source tree; otherwise the SDK is installed into DIR first. Needs cmake and a C and C++ compiler.
"""
import argparse
import json
import shutil
import subprocess
from pathlib import Path

SOURCE = Path(__file__).resolve().parents[2]
VERSION = "0.2.0"

C_SMOKE = r'''
#include <stddef.h>
#include <xgc2/module.h>
_Static_assert(XGC2_MODULE_ABI_MAJOR == 2 && XGC2_MODULE_ABI_MINOR == 0, "ABI version");
_Static_assert(XGC2_MODULE_MAX_PORTS == 64, "port capacity");
_Static_assert(sizeof(xgc2_sample_view) == 32, "sample view layout");
_Static_assert(sizeof(xgc2_step_ctx) == 32, "step context layout");
_Static_assert(offsetof(xgc2_step_ctx, changed_inputs) == 16, "changed_inputs offset");
_Static_assert(sizeof(xgc2_port_desc) == 40, "port descriptor layout");
int main(void) { return XGC2_OK; }
'''

CXX_SMOKE = r'''
#include <cstddef>
#include <type_traits>
#include <xgc2/module.h>
static_assert(std::is_standard_layout<xgc2_host_api>::value, "host API layout");
static_assert(sizeof(xgc2_sample_view) == 32, "sample view layout");
static_assert(XGC2_MODULE_ABI_MAJOR == 2u, "ABI version");
int main() {
  xgc2_module_entry_fn entry = nullptr;
  return entry != nullptr;
}
'''

CONSUMER = r'''
cmake_minimum_required(VERSION 3.10)
project(Xgc2ModuleConsumer LANGUAGES C CXX)
if(DEFINED XGC2_MODULE_SOURCE_ROOT)
  add_subdirectory("${XGC2_MODULE_SOURCE_ROOT}/sdk" sdk)
else()
  find_package(Xgc2Module ${SDK_VERSION} CONFIG REQUIRED PATHS "${SDK_PREFIX}/share/cmake/Xgc2Module" NO_DEFAULT_PATH)
endif()
get_target_property(kind Xgc2Module::SDK TYPE)
if(NOT kind STREQUAL "INTERFACE_LIBRARY")
  message(FATAL_ERROR "Xgc2Module::SDK must stay a header-only INTERFACE target")
endif()
get_target_property(links Xgc2Module::SDK INTERFACE_LINK_LIBRARIES)
if(links)
  message(FATAL_ERROR "the SDK must not bring libraries: ${links}")
endif()
add_executable(c_smoke c_smoke.c)
set_target_properties(c_smoke PROPERTIES C_STANDARD 11 C_STANDARD_REQUIRED YES C_EXTENSIONS NO)
target_compile_options(c_smoke PRIVATE -Wall -Wextra -Wpedantic -Werror)
target_link_libraries(c_smoke PRIVATE Xgc2Module::SDK)
add_executable(cxx_smoke cxx_smoke.cpp)
set_target_properties(cxx_smoke PROPERTIES CXX_STANDARD 11 CXX_STANDARD_REQUIRED YES CXX_EXTENSIONS NO)
target_compile_options(cxx_smoke PRIVATE -Wall -Wextra -Wpedantic -Werror)
target_link_libraries(cxx_smoke PRIVATE Xgc2Module::SDK)
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--work-dir', required=True, type=Path)
    parser.add_argument('--prefix', type=Path, help='check this installed prefix instead of installing from the source tree')
    args = parser.parse_args()
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=False)
    checks = []

    def run(*command, success=True, cwd=None):
        command = [str(item) for item in command]
        result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True, cwd=cwd)
        with (work / 'commands.log').open('a') as log:
            log.write(json.dumps(command) + '\n' + result.stdout + '\n')
        if (result.returncode == 0) != success:
            raise AssertionError('unexpected exit {}: {}\n{}'.format(result.returncode, command, result.stdout))
        return result.stdout

    def configure(source, build, *settings, success=True):
        # CMake 3.10 (Ubuntu 18.04) has no -S/-B, --install or --parallel: configure from the build directory.
        build.mkdir(parents=True, exist_ok=True)
        return run('cmake', *settings, source, success=success, cwd=build)

    consumer = work / 'consumer'
    consumer.mkdir()
    (consumer / 'CMakeLists.txt').write_text(CONSUMER)
    (consumer / 'c_smoke.c').write_text(C_SMOKE)
    (consumer / 'cxx_smoke.cpp').write_text(CXX_SMOKE)

    def consume(name, settings, success=True):
        build = work / ('consumer-' + name)
        configure(consumer, build, *settings, success=success)
        if not success:
            return
        run('cmake', '--build', build)
        run(build / 'c_smoke')
        run(build / 'cxx_smoke')
        checks.append(name + ': C11 and C++11 smoke builds passed with -Wall -Wextra -Wpedantic -Werror')

    if args.prefix is None:
        consume('source tree', ['-DXGC2_MODULE_SOURCE_ROOT=' + str(SOURCE)])
        prefix = work / 'prefix'
        configure(SOURCE / 'sdk', work / 'sdk-build', '-DCMAKE_INSTALL_PREFIX=' + str(prefix))
        run('cmake', '--build', work / 'sdk-build', '--target', 'install')
    else:
        prefix = args.prefix.resolve()
    expected = {'include/xgc2/module.h'} | {'share/cmake/Xgc2Module/' + name for name in
                                           ('Xgc2ModuleConfig.cmake', 'Xgc2ModuleConfigVersion.cmake', 'Xgc2ModuleTargets.cmake')}
    actual = {path.relative_to(prefix).as_posix() for path in prefix.rglob('*') if path.is_file()}
    assert expected <= actual, 'missing files: ' + str(expected - actual)
    assert (prefix / 'include/xgc2/module.h').read_bytes() == (SOURCE / 'include/xgc2/module.h').read_bytes(), 'header differs from the source'
    for path in (prefix / 'share/cmake/Xgc2Module').iterdir():
        assert str(SOURCE) not in path.read_text(), path.name + ' leaks the source path'
    consume('installed prefix', ['-DSDK_PREFIX=' + str(prefix), '-DSDK_VERSION=' + VERSION])
    relocated = work / 'relocated'
    shutil.copytree(prefix, relocated)
    consume('relocated prefix', ['-DSDK_PREFIX=' + str(relocated), '-DSDK_VERSION=' + VERSION])
    # Refusals.
    consume('other major version', ['-DSDK_PREFIX=' + str(relocated), '-DSDK_VERSION=99.0.0'], success=False)
    consume('newer minor version', ['-DSDK_PREFIX=' + str(relocated), '-DSDK_VERSION=0.99.0'], success=False)
    header = relocated / 'include/xgc2/module.h'
    original = header.read_bytes()
    header.unlink()
    try:
        build = work / 'consumer-missing-header'
        configure(consumer, build, '-DSDK_PREFIX=' + str(relocated), '-DSDK_VERSION=' + VERSION)
        run('cmake', '--build', build, success=False)
    finally:
        header.write_bytes(original)
    (relocated / 'share/cmake/Xgc2Module/Xgc2ModuleConfig.cmake').unlink()
    consume('missing package file', ['-DSDK_PREFIX=' + str(relocated), '-DSDK_VERSION=' + VERSION], success=False)
    checks.extend(['a wrong major version, a newer minor version, a missing header and a missing package file are refused'])
    print(json.dumps({'checks': checks}, indent=2))


if __name__ == '__main__':
    main()
