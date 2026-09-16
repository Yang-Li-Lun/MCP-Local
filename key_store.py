"""以目前 Windows 使用者的 DPAPI 加密保存通道金鑰。"""
import ctypes as c
from ctypes import wintypes as w
from pathlib import Path


class Blob(c.Structure):
    _fields_ = [('size', w.DWORD), ('data', c.POINTER(c.c_ubyte))]


crypt = c.WinDLL('crypt32', use_last_error=True)
kernel = c.WinDLL('kernel32', use_last_error=True)
for name in ('CryptProtectData', 'CryptUnprotectData'):
    function = getattr(crypt, name)
    function.restype = w.BOOL
    function.argtypes = [c.POINTER(Blob), c.c_void_p, c.POINTER(Blob),
                         c.c_void_p, c.c_void_p, w.DWORD, c.POINTER(Blob)]
kernel.LocalFree.argtypes = [w.HLOCAL]
kernel.LocalFree.restype = w.HLOCAL


def transform(data: bytes, decrypt: bool = False) -> bytes:
    """使用使用者範圍的加密，禁止 DPAPI 顯示互動提示。"""
    buffer = (c.c_ubyte * len(data)).from_buffer_copy(data)
    source = Blob(len(data), buffer)
    output = Blob()
    try:
        function = crypt.CryptUnprotectData if decrypt else crypt.CryptProtectData
        if not function(c.byref(source), None, None, None, None, 1, c.byref(output)):
            raise OSError('無法解密已保存的金鑰，請使用原 Windows 帳戶或重新輸入。' if decrypt
                          else 'Windows 無法加密保存金鑰，請稍後重試。')
        return c.string_at(output.data, output.size)
    finally:
        c.memset(buffer, 0, len(data))
        if output.data:
            c.memset(output.data, 0, output.size)
            kernel.LocalFree(c.cast(output.data, w.HLOCAL))


class KeyStore:
    def __init__(self, path: Path):
        self.path = path

    def load(self) -> str:
        """不存在時回傳空字串；損毀時保留原件並回報錯誤。"""
        if not self.path.exists():
            return ''
        with self.path.open('rb') as handle:
            data = handle.read(65537)
        if not data or len(data) > 65536:
            raise ValueError('已保存的金鑰檔格式錯誤，請重新輸入金鑰。')
        return transform(data, decrypt=True).decode('utf-8')

    def save(self, key: str) -> None:
        """原子替換密文；暫時清空輸入欄不刪除已保存的金鑰。"""
        if not key:
            return
        if len(key.encode('utf-8')) > 16384:
            raise ValueError('金鑰內容過長，請確認輸入。')
        encrypted = transform(key.encode('utf-8'))
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.path.with_suffix('.tmp')
        temporary.write_bytes(encrypted)
        temporary.replace(self.path)
