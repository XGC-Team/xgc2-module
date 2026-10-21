#!/usr/bin/env python3
"""Check the two packages of one distribution and architecture without rebuilding them.

    check_debs.py --distribution focal --architecture amd64 --sdk-deb A.deb --host-deb B.deb --work-dir DIR

Checks fields, the exact file list, the SDK header against the source, the SDK as a consumer sees
it (sdk/tests/test_sdk.py on the unpacked prefix), and the host binary: architecture, the highest
glibc symbol version (at most 2.27, so one build runs on Ubuntu 18.04, 20.04 and 24.04), the
usage error code and --check on the example manifest. The host package must also contain the four
frame-link modules; their Depends line is dpkg-shlibdeps of the host ELF and those modules, and on
the package's own architecture each module's entry descriptor is ABI 2.1 for its frame class.
With --installed (root, in a disposable container) the packages are also installed with dpkg and
file ownership is checked.
"""
import argparse
import ctypes
import json
import re
import subprocess
import sys
from pathlib import Path

SOURCE = Path(__file__).resolve().parents[2]
MAX_GLIBC = (2, 27)
MACHINE = {'amd64': 'Advanced Micro Devices X86-64', 'arm64': 'AArch64'}
# Basename -> (module name, tx schema, tx payload size). 8k is a different frame class, not a rename.
LINK_MODULES = {
    'libxgc2_peer_link.so': ('xgc2_peer_link', 'xgc2.frame2k.v1', 2048),
    'libxgc2_peer_link_8k.so': ('xgc2_peer_link', 'xgc2.frame8k.v1', 8192),
    'libxgc2_shm_bridge.so': ('xgc2_shm_bridge', 'xgc2.frame2k.v1', 2048),
    'libxgc2_shm_bridge_8k.so': ('xgc2_shm_bridge', 'xgc2.frame8k.v1', 8192),
}

SDK_FILES = {
    'usr/include/xgc2/module.h',
    'usr/include/xgc2/frame.h',
    'usr/include/xgc2/sdk/host.hpp',
    'usr/include/xgc2/sdk/json_config.hpp',
    'usr/include/xgc2/sdk/frame.hpp',
    'usr/include/xgc2/sdk/test_host.hpp',
    'usr/share/cmake/Xgc2Module/Xgc2ModuleConfig.cmake',
    'usr/share/cmake/Xgc2Module/Xgc2ModuleConfigVersion.cmake',
    'usr/share/cmake/Xgc2Module/Xgc2ModuleTargets.cmake',
    'usr/share/cmake/Xgc2Module/exports.map',
}
HOST_FILES = {
    'usr/bin/xgc2-module-host',
    'usr/share/xgc2/process-definitions/xgc2-module-host.json',
    'usr/share/doc/xgc2-module-host/README.md',
    'usr/share/doc/xgc2-module-host/architecture.md',
    'usr/share/doc/xgc2-module-host/manifest.md',
    'usr/share/doc/xgc2-module-host/control-api.md',
    'usr/share/doc/xgc2-module-host/copyright',
    'usr/share/doc/xgc2-module-host/examples/entity.toml',
    'usr/lib/xgc2/modules/libxgc2_peer_link.so',
    'usr/lib/xgc2/modules/libxgc2_peer_link_8k.so',
    'usr/lib/xgc2/modules/libxgc2_shm_bridge.so',
    'usr/lib/xgc2/modules/libxgc2_shm_bridge_8k.so',
}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


class _Port(ctypes.Structure):
    _fields_ = [
        ('name', ctypes.c_char_p),
        ('direction', ctypes.c_uint32),
        ('kind', ctypes.c_uint32),
        ('schema_id', ctypes.c_char_p),
        ('size', ctypes.c_uint32),
        ('align', ctypes.c_uint32),
        ('queue_depth', ctypes.c_uint32),
        ('flags', ctypes.c_uint32),
    ]


class _Desc(ctypes.Structure):
    _fields_ = [
        ('abi_major', ctypes.c_uint32),
        ('abi_minor', ctypes.c_uint32),
        ('name', ctypes.c_char_p),
        ('version', ctypes.c_char_p),
        ('ports', ctypes.POINTER(_Port)),
        ('port_count', ctypes.c_uint32),
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--distribution', required=True)
    parser.add_argument('--architecture', required=True, choices=sorted(MACHINE))
    parser.add_argument('--sdk-deb', required=True, type=Path)
    parser.add_argument('--host-deb', required=True, type=Path)
    parser.add_argument('--work-dir', required=True, type=Path)
    parser.add_argument('--installed', action='store_true')
    args = parser.parse_args()
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=False)
    checks = []

    def run(*command, success=True, cwd=None):
        command = [str(item) for item in command]
        result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True, cwd=cwd)
        with (work / 'commands.log').open('a') as log:
            log.write(json.dumps(command) + '\n' + result.stdout + result.stderr + '\n')
        require((result.returncode == 0) == success, 'unexpected exit {}: {}\n{}{}'.format(result.returncode, command, result.stdout, result.stderr))
        return result.stdout.strip()

    metadata = (SOURCE / '.xgc2/product.yml').read_text()
    version = re.search(r'^    {}: (\S+)$'.format(args.distribution), metadata, re.M).group(1)
    expected_fields = {
        args.sdk_deb: {'Package': 'libxgc2-module-dev', 'Version': version, 'Architecture': 'all', 'Depends': ''},
        args.host_deb: {'Package': 'xgc2-module-host', 'Version': version, 'Architecture': args.architecture},
    }
    for deb, expected in expected_fields.items():
        fields = {field: run('dpkg-deb', '-f', deb, field) for field in expected}
        require(fields == expected, '{}: unexpected control fields {}'.format(deb.name, fields))
    checks.append('control fields: package, version {}, architecture; SDK has no Depends'.format(version))

    trees = {}
    for deb, expected in ((args.sdk_deb, SDK_FILES), (args.host_deb, HOST_FILES)):
        tree = work / deb.stem
        run('dpkg-deb', '-x', deb, tree)
        actual = {path.relative_to(tree).as_posix() for path in tree.rglob('*') if not path.is_dir()}
        require(actual == expected, '{}: file list differs: {}'.format(deb.name, sorted(actual ^ expected)))
        trees[deb] = tree
    checks.append('file lists of both packages are exact')

    sdk = trees[args.sdk_deb]
    require((sdk / 'usr/include/xgc2/module.h').read_bytes() == (SOURCE / 'include/xgc2/module.h').read_bytes(), 'packaged header differs from include/xgc2/module.h')
    run(sys.executable, '-B', SOURCE / 'sdk/tests/test_sdk.py', '--work-dir', work / 'sdk-consumer', '--prefix', sdk / 'usr')
    checks.append('SDK: header equals the source; C11 and C++11 consumers build against the unpacked prefix and a relocated copy')

    host = trees[args.host_deb]
    binary = host / 'usr/bin/xgc2-module-host'
    header = run('readelf', '-h', binary)
    require(MACHINE[args.architecture] in header, 'wrong machine type for ' + args.architecture)
    versions = set(re.findall(r'GLIBC_(\d+)\.(\d+)', run('objdump', '-T', binary)))
    highest = max((int(a), int(b)) for a, b in versions)
    require(highest <= MAX_GLIBC, 'the binary needs glibc {}.{}; at most {}.{} keeps it usable on Ubuntu 18.04'.format(*highest, *MAX_GLIBC))
    checks.append('host binary: {}, needs glibc {}.{} at most'.format(MACHINE[args.architecture], *highest))

    # Depends is whatever this distro's dpkg-shlibdeps reports for the host ELF and the four modules.
    # The modules are built in the distro image, so their glibc and libjsoncpp requirements are that
    # distro's; the 2.27 cap above stays on the one host ELF.
    link_elves = [host / 'usr/lib/xgc2/modules' / name for name in sorted(LINK_MODULES)]
    debian = work / 'debian'
    debian.mkdir()
    (debian / 'control').write_text('Source: xgc2-module\n\nPackage: xgc2-module-host\nArchitecture: any\n')
    depends_lines = [line.split('=', 1)[1] for line in run('dpkg-shlibdeps', '-O', binary, *link_elves, cwd=work).splitlines() if line.startswith('shlibs:Depends=')]
    require(len(depends_lines) == 1, 'dpkg-shlibdeps Depends lines: {}'.format(depends_lines))
    require(run('dpkg-deb', '-f', args.host_deb, 'Depends') == depends_lines[0], 'host Depends is not dpkg-shlibdeps of the packaged ELFs: ' + depends_lines[0])
    require('libjsoncpp.so' in run('objdump', '-p', link_elves[0]), 'frame link does not need libjsoncpp')
    checks.append('host Depends matches dpkg-shlibdeps of the host ELF and the four frame links')

    native = subprocess.run(['uname', '-m'], stdout=subprocess.PIPE, universal_newlines=True).stdout.strip() == {'amd64': 'x86_64', 'arm64': 'aarch64'}[args.architecture]
    for link in link_elves:
        symbols = run('readelf', '-W', '--dyn-syms', link)
        defined = []
        for line in symbols.splitlines():
            columns = line.split()
            if len(columns) >= 8 and columns[3] == 'FUNC' and columns[4] == 'GLOBAL' and columns[6] != 'UND':
                defined.append(columns[-1].split('@')[0])
        require(defined == ['xgc2_module_entry'], '{} exports {}: version script exports only the entry'.format(link.name, defined))
        require(MACHINE[args.architecture] in run('readelf', '-h', link), link.name + ' has the wrong machine')
        if not native:
            continue
        # The entry fills the port table. ABI 2.1 and the frame class are what a host of this SDK accepts.
        loaded = ctypes.CDLL(str(link), mode=ctypes.RTLD_LOCAL)
        loaded.xgc2_module_entry.restype = ctypes.POINTER(_Desc)
        desc = loaded.xgc2_module_entry().contents
        module_name, schema, size = LINK_MODULES[link.name]
        require((desc.abi_major, desc.abi_minor, desc.port_count) == (2, 1, 3),
                '{} descriptor is ABI {}.{} with {} ports'.format(link.name, desc.abi_major, desc.abi_minor, desc.port_count))
        require(desc.name.decode() == module_name and desc.version.decode() == '1.0.0',
                '{} identity {} {}'.format(link.name, desc.name, desc.version))
        tx, rx, stats = (desc.ports[index] for index in range(3))
        require(tx.schema_id.decode() == schema and tx.size == size, '{} tx is {} {}'.format(link.name, tx.schema_id, tx.size))
        require(rx.schema_id.decode() == schema and rx.size == size, '{} rx frame class differs from tx'.format(link.name))
        require(stats.schema_id.decode() == 'xgc2.link_stats.v1' and stats.size == 144, '{} stats port is not link_stats'.format(link.name))
        del loaded
    if native:
        checks.append('four frame links: ELF, entry symbol, ABI 2.1, and the 2k or 8k frame class')
    else:
        checks.append('four frame links: ELF and entry symbol; ABI call skipped on a foreign architecture')
    if native:
        run(binary, success=False)
        example = host / 'usr/share/doc/xgc2-module-host/examples/entity.toml'
        manifest = work / 'check.toml'
        manifest.write_text('entity = "package-check"\n')
        report = json.loads(run(binary, '--manifest', manifest, '--check'))
        require(report['entity'] == 'package-check', 'unexpected --check report')
        # The shipped example names libraries that are not installed: it must fail on those, not on syntax.
        refused = subprocess.run([str(binary), '--manifest', str(example), '--check'], stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True)
        require(refused.returncode == 2 and 'libugv_ros_edge.so' in refused.stderr, 'example manifest: ' + refused.stderr)
        checks.append('host binary runs: usage error, --check on a manifest, the example manifest parses and names missing libraries')
    else:
        checks.append('host binary run skipped: foreign architecture')

    if args.installed:
        run('dpkg', '-i', args.sdk_deb, args.host_deb)
        for deb, expected in ((args.sdk_deb, SDK_FILES), (args.host_deb, HOST_FILES)):
            package = expected_fields[deb]['Package']
            for path in sorted(expected):
                owner = run('dpkg-query', '-S', '/' + path)
                require(owner.startswith(package + ': '), 'ownership mismatch: ' + owner)
        checks.append('installed with dpkg: every file is owned by its package')

    print(json.dumps({'checks': checks, 'distribution': args.distribution, 'architecture': args.architecture}, indent=2))


if __name__ == '__main__':
    main()
