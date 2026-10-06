"""唯讀偵測已安裝工具；不執行 PATH 程式，不映射使用者設定或工具快取。"""
from __future__ import annotations

import ctypes
import os
from pathlib import Path
import shutil
import stat
import sys
import uuid

from local_files_mcp import linked

MAX_TOOLCHAINS = 16
CREDENTIAL_NAMES = frozenset({
    '.ssh', '.aws', '.azure', '.gnupg', '.kube', '.docker', '.config',
    '.git', '.gitconfig', '.git-credentials', '.npmrc', '.pypirc', '.netrc',
    '.env', 'credentials', 'credentials.json', 'credentials.toml',
    'api-key.dpapi', 'id_rsa', 'id_ed25519',
})


def known_folders() -> dict[str, Path]:
    """由 Windows Known Folder API 取得邊界，不信任可覆寫的環境變數。"""
    if os.name != 'nt':
        return {}
    identifiers = {
        'program_files': '905e63b6-c1bf-494e-b29c-65b732d3d21a',
        'program_files_x86': '7c5a40ef-a0fb-4bfc-874a-c0f2e0b9fa8e',
        'local_app_data': 'f1b32785-6fba-4fcf-9d55-7b8e7f157091',
        'profile': '5e6c858f-0e22-4760-9afe-ea3317b67173',
        'windows': 'f38bf404-1d43-42f2-9305-67de0b28fc23',
    }
    shell = ctypes.WinDLL('shell32')
    resolve = shell.SHGetKnownFolderPath
    resolve.argtypes = [ctypes.c_void_p, ctypes.c_uint32, ctypes.c_void_p,
                       ctypes.POINTER(ctypes.c_void_p)]
    resolve.restype = ctypes.c_long
    free = ctypes.WinDLL('ole32').CoTaskMemFree
    free.argtypes = [ctypes.c_void_p]
    result = {}
    for name, identifier in identifiers.items():
        guid = (ctypes.c_ubyte * 16).from_buffer_copy(uuid.UUID(identifier).bytes_le)
        pointer = ctypes.c_void_p()
        if resolve(guid, 0, None, ctypes.byref(pointer)) == 0:
            try:
                result[name] = Path(ctypes.wstring_at(pointer)).absolute()
            finally:
                free(pointer)
    return result


def installation_specs(folders: dict[str, Path]) -> list[tuple[str, Path, str]]:
    """僅列出產品安裝子樹；Rust 不使用含 credentials.toml 的 .cargo。"""
    result = []
    # The APP's verified, repository-local Python distribution is a runtime
    # subtree, never the repository or venv/site-packages itself.
    bundled = Path(sys.base_prefix).absolute()
    project = Path(__file__).resolve().parent
    if (bundled.parent == project / '.venv-runtime'
            and bundled.name.startswith('Python3')):
        result.append(('python', bundled, 'python.exe'))
    layouts = (
        ('git', 'Git', 'cmd/git.exe'), ('node', 'nodejs', 'node.exe'),
        ('go', 'Go', 'bin/go.exe'), ('cmake', 'CMake', 'bin/cmake.exe'),
        ('clang', 'LLVM', 'bin/clang.exe'), ('gcc', 'mingw64', 'bin/gcc.exe'),
    )
    for key in ('program_files', 'program_files_x86'):
        if key not in folders:
            continue
        base = folders[key]
        result.extend((kind, base / directory, executable)
                      for kind, directory, executable in layouts)
        for directory in sorted(base.glob('Python3*'))[:16]:
            result.append(('python', directory, 'python.exe'))
    local = folders.get('local_app_data')
    if local:
        for directory in sorted((local / 'Programs/Python').glob('Python3*'))[:16]:
            result.append(('python', directory, 'python.exe'))
        result.extend((kind, local / 'Programs' / directory, executable)
                      for kind, directory, executable in layouts)
    profile = folders.get('profile')
    if profile:
        for directory in sorted((profile / '.rustup/toolchains').glob('*-pc-windows-*'))[:16]:
            result.append(('rust', directory, 'bin/rustc.exe'))
    windows = folders.get('windows')
    if windows:
        drive = Path(windows.anchor)
        for directory in ('msys64/mingw64', 'msys64/ucrt64', 'mingw64'):
            result.append(('gcc', drive / directory, 'bin/gcc.exe'))
        result.append(('clang', drive / 'msys64/clang64', 'bin/clang.exe'))
    return result


def validate_toolchain_boundary(path: Path) -> None:
    """拒絕整個磁碟、Known Folder、設定樹與重新解析點。"""
    path = Path(path)
    if (not path.is_absolute() or path.drive.startswith('\\\\') or '..' in path.parts
            or not path.is_dir() or any(linked(part) for part in (path, *path.parents))):
        raise ValueError('VM_TOOLCHAIN_PATH：工具鏈需要無連結的本機安裝目錄。')
    resolved = path.resolve(strict=True)
    if any(part.casefold() in CREDENTIAL_NAMES for part in resolved.parts):
        raise ValueError('VM_TOOLCHAIN_CREDENTIALS：不可映射憑證或設定目錄。')
    boundaries = [Path(resolved.anchor), *known_folders().values()]
    if any(boundary == resolved or boundary.is_relative_to(resolved) for boundary in boundaries):
        raise ValueError('VM_TOOLCHAIN_SCOPE：不可映射磁碟或整個使用者／系統目錄。')


def discover_toolchains(roots: list[Path] | None = None) -> list[str]:
    """以實際 executable 核對固定安裝布局，跳過不安全或與授權 root 重疊的候選。"""
    if os.name != 'nt':
        return []
    from developer_control import validate_mapping_tree
    import security_policy
    folders = known_folders()
    protected = [security_policy.STATE_DIR.resolve(), *(Path(p).resolve() for p in roots or [])]
    result: list[Path] = []
    # PATH 只用來排序已經落在可信安裝布局的 executable，絕不映射 PATH 目錄本身。
    executables = ('python.exe', 'git.exe', 'node.exe', 'go.exe', 'rustc.exe',
                   'cargo.exe', 'cmake.exe', 'gcc.exe', 'clang.exe')
    preferred = {Path(value).absolute() for name in executables
                 if (value := shutil.which(name))}
    try:
        specs = installation_specs(folders)
    except OSError:
        return []
    specs.sort(key=lambda item: item[1] / item[2] not in preferred)
    for kind, raw, executable in specs:
        try:
            path = raw.resolve(strict=True)
            actual = raw / executable
            info = actual.lstat()
            if (not stat.S_ISREG(info.st_mode) or getattr(info, 'st_file_attributes', 0) & 0x400
                    or actual.resolve(strict=True) != actual.absolute()
                    or kind == 'rust' and not (path / 'bin/cargo.exe').is_file()):
                continue
            validate_toolchain_boundary(raw)
            if any(path.is_relative_to(p) or p.is_relative_to(path) for p in protected + result):
                continue
            validate_mapping_tree(path, protect_credentials=True)
        except (OSError, ValueError):
            continue  # No extra tools is valid: the guest still has PowerShell.
        result.append(path)
        if len(result) >= MAX_TOOLCHAINS:
            break
    return [str(path) for path in result]


def safe_builtin_npm_config(path: Path, root: Path) -> bool:
    """只接受 npm 隨程式附帶的固定 prefix；任何 auth/私人值仍拒絕映射。"""
    if path.relative_to(root).as_posix().casefold() != 'node_modules/npm/.npmrc':
        return False
    try:
        # Alias accounting has already completed before this bounded read.
        with path.open('rb') as handle:
            raw = handle.read(1025)
        return len(raw) <= 1024 and raw.strip() in (b'', b'prefix=${APPDATA}\\npm', b'prefix=${APPDATA}/npm')
    except (OSError, ValueError):
        return False
