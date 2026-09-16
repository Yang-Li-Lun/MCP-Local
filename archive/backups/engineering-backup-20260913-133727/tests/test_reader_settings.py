"""進階設定的遷移、傳遞及實際存取規則。"""
import json
from pathlib import Path
import tempfile
import unittest

from local_files_gui import build_commands, load_settings, save_settings
from local_files_mcp import FileReader
from reader_settings import normalize_reader_settings, encode_reader_settings, decode_reader_settings


class ReaderSettingsTests(unittest.TestCase):
    def test_old_configuration_migrates_without_losing_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'settings.json'
            old = {'root': directory, 'tunnel': 'tunnel_old', 'recent': [directory], 'start_hidden': True}
            path.write_text(json.dumps(old), encoding='utf-8')
            loaded = load_settings(path)
            self.assertEqual(loaded['reader'], normalize_reader_settings())
            for key, value in old.items():
                self.assertEqual(loaded[key], str(Path(value).resolve()) if key == 'root' else value)
            loaded['reader']['max_scan_files'] = 17
            loaded['key'] = 'fake-key-never-save'
            save_settings(loaded, path)
            self.assertEqual(load_settings(path)['reader']['max_scan_files'], 17)
            self.assertNotIn('fake-key-never-save', path.read_text())

    def test_command_carries_validated_snapshot(self):
        settings = {'root': str(Path('.').resolve()), 'tunnel': 'tunnel_test',
                    'reader': {'max_scan_files': 123, 'extensions': ['.ABC'], 'excluded_names': ['私用資料']}}
        commands = build_commands(settings)
        command = commands[0][commands[0].index('--mcp-command') + 1]
        snapshot = command.split(' --reader-settings ')[1]
        settings['reader']['max_scan_files'] = 1
        decoded = decode_reader_settings(snapshot)
        self.assertEqual(decoded['max_scan_files'], 123)
        self.assertEqual(decoded['extensions'], ['.abc'])
        self.assertEqual(decoded['excluded_names'], ['私用資料'])

    def test_rejects_invalid_and_unbounded_values(self):
        invalid = [{'max_file_bytes': 0}, {'max_scan_files': True},
                   {'max_scan_entries': 300001}, {'max_scan_bytes': 1.5},
                   {'extensions': ['*.txt']}, {'excluded_names': ['../private']},
                   {'text_names': ['']}, {'extensions': '.txt'}, {'allow_write': True}]
        for settings in invalid:
            with self.subTest(settings=settings), self.assertRaises(ValueError):
                normalize_reader_settings(settings)
        with self.assertRaises(ValueError):
            decode_reader_settings('broken%')

    def test_custom_extensions_names_and_exclusions_take_effect(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ('sample.custom', 'sample.txt', 'SPECIAL', '.hidden.custom'):
                (root / name).write_text('marker', encoding='utf-8')
            (root / 'build').mkdir()
            (root / 'build' / 'allowed.custom').write_text('marker', encoding='utf-8')
            reader = FileReader(root, {'extensions': ['custom'], 'text_names': ['special'], 'excluded_names': []})
            self.assertEqual(set(reader.list_files()['files']), {'sample.custom', 'SPECIAL'})
            self.assertIn('marker', reader.read_file('sample.custom')['content'])
            self.assertEqual(len(reader.search_text('marker')['matches']), 2)
            blocked = FileReader(root, {'extensions': ['custom'], 'excluded_names': ['sample.custom', 'build']})
            self.assertEqual(blocked.list_files()['files'], [])
            for name in ('sample.custom', 'build/allowed.custom', '.hidden.custom', '../outside.custom'):
                with self.subTest(name=name), self.assertRaises(ValueError):
                    blocked.read_file(name)

    def test_extension_change_does_not_allow_binary_or_non_utf8(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'binary.custom').write_bytes(b'abc\x00def')
            (root / 'bad.custom').write_bytes(b'\xff')
            reader = FileReader(root, {'extensions': ['custom'], 'excluded_names': []})
            for name in ('binary.custom', 'bad.custom'):
                with self.subTest(name=name), self.assertRaises(ValueError):
                    reader.read_file(name)
