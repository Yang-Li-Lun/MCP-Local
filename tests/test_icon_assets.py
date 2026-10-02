"""圖示偏好相容性與 Windows 圖示資源生命週期。"""
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from PIL import Image
from connection_settings import load_settings, save_settings
from icon_assets import ICON_STYLES, icon_path
from tray_windows import NotifyIcon, Tray


class IconTests(unittest.TestCase):
    def test_default_roundtrip_and_backup(self):
        with tempfile.TemporaryDirectory() as folder:
            target = Path(folder) / 'settings.json'
            settings = {'root': folder, 'tunnel': 'tunnel_fixture'}
            save_settings(settings, target)
            self.assertEqual(load_settings(target)['icon_style'], 'classic')
            settings['icon_style'] = 'folder_link'
            save_settings(settings, target)
            self.assertEqual(load_settings(target)['icon_style'], 'folder_link')
            settings['icon_style'] = 'classic'
            save_settings(settings, target)
            backups = [json.loads(p.read_text(encoding='utf-8')) for p in target.parent.glob('*.bak')]
            self.assertIn('folder_link', [b['icon_style'] for b in backups])
            original = target.read_bytes()
            for invalid in ('../outside.ico', '', None, [], 1):
                settings['icon_style'] = invalid
                with self.assertRaises(ValueError):
                    save_settings(settings, target)
                self.assertEqual(target.read_bytes(), original)

    def test_bundled_assets_decode(self):
        for style in ICON_STYLES:
            with Image.open(icon_path(style)) as icon:
                self.assertEqual(icon.ico.sizes(), {(s, s) for s in (16, 24, 32, 48, 64, 128, 256)})
                for size in icon.ico.sizes():
                    icon.ico.getimage(size).load()
            with Image.open(icon_path(style, preview=True)) as preview:
                self.assertEqual(preview.size, (512, 512))
                self.assertEqual(preview.mode, 'RGBA')

    def test_tray_replaces_handle_only_after_success(self):
        tray = Tray.__new__(Tray)
        tray.icon_style, tray.icon = 'classic', 101
        tray.data = NotifyIcon(icon=101)
        with patch('tray_windows.user.LoadImageW', return_value=202), patch(
                'tray_windows.shell.Shell_NotifyIconW', return_value=0), patch(
                'tray_windows.user.DestroyIcon') as destroy:
            with self.assertRaises(OSError):
                tray.set_icon('folder_link')
            self.assertEqual((tray.icon, tray.data.icon, tray.icon_style), (101, 101, 'classic'))
            destroy.assert_called_once_with(202)
        with patch('tray_windows.user.LoadImageW', return_value=303), patch(
                'tray_windows.shell.Shell_NotifyIconW', return_value=1), patch(
                'tray_windows.user.DestroyIcon') as destroy:
            tray.set_icon('folder_link')
            self.assertEqual((tray.icon, tray.data.icon, tray.icon_style), (303, 303, 'folder_link'))
            destroy.assert_called_once_with(101)
