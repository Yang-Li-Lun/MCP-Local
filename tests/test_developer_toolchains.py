"""自動工具鏈偵測與唯讀映射邊界；只使用合成安裝樹。"""
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

from developer_control import build_vm_mapping_plan
from developer_toolchains import discover_toolchains, validate_toolchain_boundary


class DeveloperToolchainTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='mcp-auto-toolchains-')
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.folders = {name: self.base / name for name in
                        ('program_files', 'local_app_data', 'profile')}
        for folder in self.folders.values():
            folder.mkdir()
        self.state = self.base / 'state'
        self.state.mkdir()
        self.root = self.base / 'authorized'
        self.root.mkdir()
        for target, value in (('developer_toolchains.known_folders', self.folders),
                              ('developer_control.security_policy.STATE_DIR', self.state),
                              ('developer_toolchains.sys.base_prefix', str(self.base / 'fixture-runtime'))):
            mock = patch(target, return_value=value) if 'known_folders' in target else patch(target, value)
            mock.start()
            self.addCleanup(mock.stop)

    def executable(self, relative):
        path = self.base / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b'synthetic executable; never executed')
        return path

    def test_all_tool_families_detected_and_mapped_readonly(self):
        paths = ('program_files/Python313/python.exe', 'program_files/Git/cmd/git.exe',
                 'program_files/nodejs/node.exe', 'program_files/Go/bin/go.exe',
                 'program_files/CMake/bin/cmake.exe', 'program_files/LLVM/bin/clang.exe',
                 'program_files/mingw64/bin/gcc.exe',
                 'profile/.rustup/toolchains/stable-x86_64-pc-windows-msvc/bin/rustc.exe',
                 'profile/.rustup/toolchains/stable-x86_64-pc-windows-msvc/bin/cargo.exe')
        for path in paths:
            self.executable(path)
        tools = discover_toolchains([self.root])
        self.assertEqual(len(tools), 8)
        xml = ET.fromstring(build_vm_mapping_plan([self.root], [Path(p) for p in tools]))
        self.assertEqual([p.text for p in xml.findall('MappedFolders/MappedFolder/ReadOnly')],
                         ['false'] + ['true'] * 8)
        self.assertNotIn(str(self.folders['profile'] / '.cargo'), tools)

    def test_path_and_environment_do_not_authorize_arbitrary_folders(self):
        untrusted = self.executable('untrusted/python.exe')
        with patch('developer_toolchains.shutil.which', return_value=str(untrusted)), patch.dict(
                os.environ, {'ProgramFiles': str(untrusted.parent), 'CARGO_HOME': str(self.base)}):
            self.assertEqual(discover_toolchains([self.root]), [])

    def test_credentials_overlap_and_aliases_are_skipped(self):
        executable = self.executable('program_files/nodejs/node.exe')
        (executable.parent / '.npmrc').write_text('SYNTHETIC_NOT_SECRET')
        self.assertEqual(discover_toolchains(), [])
        (executable.parent / '.npmrc').unlink()
        self.assertEqual(discover_toolchains([executable.parent]), [])
        external = self.executable('outside/node.exe')
        os.link(external, executable.parent / 'external-link.exe')
        self.assertEqual(discover_toolchains(), [])
        with self.assertRaisesRegex(ValueError, 'VM_MAPPING_ALIAS'):
            build_vm_mapping_plan([self.root], [executable.parent])

    def test_whole_profile_drive_and_credential_subtrees_refused(self):
        ssh = self.folders['profile'] / '.ssh'
        ssh.mkdir()
        for path in (self.folders['profile'], self.folders['program_files'], self.base,
                     Path(self.base.anchor), ssh):
            with self.subTest(path=path), self.assertRaises(ValueError):
                validate_toolchain_boundary(path)

    def test_credential_added_after_detection_is_rejected_before_mapping(self):
        executable = self.executable('program_files/nodejs/node.exe')
        self.assertEqual(discover_toolchains(), [str(executable.parent)])
        (executable.parent / 'credentials.toml').write_text('SYNTHETIC_NOT_SECRET')
        with self.assertRaisesRegex(ValueError, 'VM_TOOLCHAIN_CREDENTIALS'):
            build_vm_mapping_plan([self.root], [executable.parent])

    def test_builtin_npm_config_allows_only_empty_or_fixed_prefix(self):
        executable = self.executable('program_files/nodejs/node.exe')
        config = executable.parent / 'node_modules/npm/.npmrc'
        config.parent.mkdir(parents=True)
        for raw in (b'', b'prefix=${APPDATA}\\npm\n'):
            config.write_bytes(raw)
            self.assertEqual(discover_toolchains(), [str(executable.parent)])
        for raw in (b'//example.invalid/:_authToken=SYNTHETIC', b'prefix=C:/Users/private', b'# SYNTHETIC_SECRET'):
            config.write_bytes(raw)
            self.assertEqual(discover_toolchains(), [])
