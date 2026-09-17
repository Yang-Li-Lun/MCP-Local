"""資料夾簡化、設定 v5 與雙態介面的安全回歸。"""
import json
from pathlib import Path
import tempfile
import time
import tkinter as tk
import unittest
from unittest.mock import Mock, patch
from connection_settings import normalize_connection, save_settings, load_settings
from workspace_settings import create_workspace_root, allocate_root_id, normalize_workspace
from power_policy import PowerPolicyManager, PowerMode
from local_files_gui import App


class FolderPowerGuiTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.folders = []
        for i in range(9):
            path = self.base / ('folder' + str(i))
            path.mkdir()
            self.folders.append(path)
        self.rows = [create_workspace_root(str(self.folders[0]), [])]
        self.settings = normalize_connection({'roots': self.rows, 'default_root': self.rows[0]['id'],
                                              'tunnel': 'tunnel_fixture', 'settings_version': 3})

    def test_ids_capacity_duplicates_parents_and_preserved_exclusions(self):
        rows = []
        for path in self.folders[:8]:
            rows.append(create_workspace_root(str(path), rows))
        self.assertEqual(len({row['id'] for row in rows}), 8)
        for row in rows:
            self.assertRegex(row['id'], r'^root_[0-9a-f]{12}$')
            self.assertEqual(row['excluded_names'], [])
        with self.assertRaisesRegex(ValueError, '8'):
            create_workspace_root(str(self.folders[8]), rows)
        nested = self.folders[0] / 'nested'
        nested.mkdir()
        for path in (self.folders[0], nested, self.base):
            with self.assertRaises(ValueError):
                create_workspace_root(str(path), self.rows)
        previous = dict(self.rows[0], excluded_names=['private'])
        replacement = create_workspace_root(str(self.folders[1]), [previous], previous=previous)
        self.assertEqual(replacement['id'], previous['id'])
        self.assertEqual(replacement['excluded_names'], ['private'])
        self.assertEqual(replacement['name'], 'folder1')

    def test_new_root_safety_and_settings_boundary(self):
        for path in (self.base / 'missing', Path.home(), Path('relative')):
            with self.assertRaises(ValueError):
                create_workspace_root(str(path), [])
        with patch('connection_settings.CONFIG_DIR', self.folders[0] / 'settings'):
            with self.assertRaisesRegex(ValueError, '設定'):
                create_workspace_root(str(self.folders[0]), [])

    def test_v1_v2_v3_migrate_to_off_v4_once(self):
        target = self.base / 'settings.json'
        for version in (1, 2, 3):
            old = dict(self.settings, settings_version=version, power={'mode': 'extreme'})
            target.write_text(json.dumps(old), encoding='utf-8')
            value = load_settings(target)
            self.assertEqual(value['settings_version'], 5)
            self.assertEqual(value['power']['mode'], 'off')
            self.assertEqual(value['roots'], self.rows)
            before = target.read_bytes()
            load_settings(target)
            self.assertEqual(target.read_bytes(), before)
        legacy = normalize_connection(dict(self.settings, settings_version=0))
        self.assertEqual(legacy['settings_version'], 0)

    def test_v4_modes_backup_whitelist_and_conflict(self):
        target = self.base / 'settings.json'
        for mode in ('off', 'extreme'):
            settings = dict(self.settings, power={'mode': mode})
            save_settings(settings, target)
            self.assertEqual(load_settings(target)['power']['mode'], mode)
        old = json.loads(target.read_text(encoding='utf-8'))
        old['power']['api_key'] = 'DO_NOT_BACKUP'
        target.write_text(json.dumps(old), encoding='utf-8')
        with self.assertRaises(ValueError):
            save_settings(self.settings, target)
        self.assertEqual(json.loads(target.read_text(encoding='utf-8')), old)

    def create_app(self):
        window = tk.Tk()
        window.withdraw()
        with patch('tray_windows.Tray'):
            app = App(window, self.settings)
        window.update_idletasks()
        self.addCleanup(app.finish)
        return app

    def poll(self, app):
        deadline = time.monotonic() + 3
        while app.tasks.busy and time.monotonic() < deadline:
            app.tasks.poll()
            time.sleep(.005)
        self.assertFalse(app.tasks.busy)

    def test_gui_has_only_folder_paths_and_empty_draft_is_blocked(self):
        app = self.create_app()
        self.assertEqual(app.folder_list.get(0), str(self.folders[0]))
        for field in ('root', 'roots', 'workspace_fields', 'workspace_default', 'workspace_choice'):
            self.assertFalse(hasattr(app, field))
        self.assertEqual(len(app.tabs.tabs()), 2)
        app.folder_list.selection_set(0)
        app.remove_selected_folders()
        self.assertEqual(app.workspace_rows, [])
        self.assertFalse(app.control_states()['save'])
        self.assertFalse(app.control_states()['start'])
        with patch('local_files_gui.messagebox.showerror'):
            self.assertFalse(app.save())

    def test_gui_changes_preserve_identity_and_save_order(self):
        app = self.create_app()
        app.workspace_rows[0]['excluded_names'] = ['private']
        identifier = app.workspace_rows[0]['id']
        app.folder_list.selection_set(0)
        with patch('local_files_gui.filedialog.askdirectory', return_value=str(self.folders[1])):
            app.change_selected_folder()
        self.assertEqual(app.workspace_rows[0]['id'], identifier)
        self.assertEqual(app.workspace_rows[0]['excluded_names'], ['private'])
        with patch('local_files_gui.filedialog.askdirectory', return_value=str(self.folders[2])):
            app.add_folder()
        target = self.base / 'settings.json'
        with patch('local_files_gui.save_settings', side_effect=lambda v: save_settings(v, target)), \
             patch('local_files_gui.get_background_status', return_value=None):
            app.save()
            self.poll(app)
        loaded = load_settings(target)
        self.assertEqual(loaded['default_root'], identifier)
        self.assertEqual([r['path'] for r in loaded['roots']], [str(p) for p in self.folders[1:3]])
        self.assertEqual(loaded['roots'][0]['excluded_names'], ['private'])

    def test_running_mode_change_does_not_restart_and_failure_rolls_back_ui(self):
        app = self.create_app()
        # Mock native boundaries, retaining the real state machine.
        api = Mock()
        guard = Mock(apply=Mock(return_value=[]))
        app.power = PowerPolicyManager(self.base / 'power.json', api=api, guard=guard)
        app.running, app.state = True, 'RUNNING'
        with patch.object(app.connection, 'start') as start, patch.object(app.connection, 'stop') as stop:
            app.change_power_mode('off')
            self.poll(app)
            self.assertEqual(app.power_mode.get(), 'off')
            app.change_power_mode('extreme')
            self.poll(app)
            self.assertEqual(app.power_mode.get(), 'extreme')
            start.assert_not_called()
            stop.assert_not_called()
        with patch.object(app.power, 'apply', side_effect=RuntimeError('fixture')), \
             patch('local_files_gui.messagebox.showerror'):
            app.change_power_mode('off')
            self.poll(app)
        self.assertEqual(app.power_mode.get(), 'extreme')
        self.assertEqual(app.power.mode, PowerMode.EXTREME)
        app.running = False

    def test_repair_rows_do_not_expose_internal_ids(self):
        app = self.create_app()
        app.invalid_root_ids = {app.workspace_rows[0]['id']}
        app.refresh_folder_list()
        self.assertIn('無法使用', app.folder_list.get(0))
        self.assertNotIn(app.workspace_rows[0]['id'], app.folder_list.get(0))

    def test_three_root_routing_for_all_tools(self):
        from workspace_reader import WorkspaceReader
        roots = []
        for index, path in enumerate(self.folders[:3]):
            (path / 'README.md').write_text('marker' + str(index), encoding='utf-8')
            roots.append(create_workspace_root(str(path), roots))
        reader = WorkspaceReader(roots, roots[0]['id'])
        self.assertEqual(len(reader.workspace_info()['roots']), 3)
        for index, root in enumerate(roots):
            identifier = root['id']
            for method, arguments in [('read_file', {'path': 'README.md'}),
                    ('read_files', {'files': [{'path': 'README.md'}]}),
                    ('search_text', {'query': 'marker'}), ('search_texts', {'queries': ['marker']}),
                    ('project_context', {}), ('list_directory', {}), ('list_files', {}), ('list_projects', {})]:
                with self.subTest(root=index, tool=method):
                    result = getattr(reader, method)(**arguments, root_id=identifier)
                    self.assertNotIn(str(self.base), str(result))
                    if method in ('read_file', 'read_files', 'search_text', 'search_texts', 'project_context'):
                        self.assertIn('marker' + str(index), str(result))
                        for other in range(3):
                            if other != index:
                                self.assertNotIn('marker' + str(other), str(result))
        remaining = WorkspaceReader(roots[1:], roots[1]['id'])
        self.assertIn('marker1', remaining.read_file('README.md')['content'])
