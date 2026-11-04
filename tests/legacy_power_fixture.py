"""Synthetic generator for old recovery journals; never used by the application."""
from mcp_local.power_policy import PowerSchemeManager, PowerMode, PowerError
from mcp_local.power_windows import GUID_POWER_MODE_BEST_EFFICIENCY
import json
from pathlib import Path
import os

SLEEP = ('238c9fa8-0aad-41ed-83f4-97be242c8f20', '29f6c1db-86da-48c5-9fdb-f2b67b1f44da')
DISPLAY = ('7516b95f-f776-4464-8c53-06167f40cc99', '3c0bc021-c8a8-4e07-a973-6b14cbcb2b7e')
CPU = '54533251-82be-4824-96c1-47b60b740d00'
EPP = (CPU, '36687f9e-e3a5-4dbf-b1dc-15eb381c6863')
MAXIMUM = (CPU, 'bc5038f7-23e0-4960-96da-33abaf5935ec')
BOOST = (CPU, 'be337238-0d82-4146-a960-4f3749d470c7')


class LegacySchemeFixture(PowerSchemeManager):
    def journal(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        import tempfile
        if self.path.is_symlink():
            raise PowerError('POWER_RUNTIME_STATE_CORRUPT')
        descriptor, name = tempfile.mkstemp(prefix=self.path.name + '.', suffix='.tmp',
                                            dir=self.path.parent)
        temporary = Path(name)
        try:
            with os.fdopen(descriptor, 'w', encoding='utf-8') as stream:
                json.dump(self.state, stream)
                stream.flush()
                os.fsync(stream.fileno())
            temporary.replace(self.path)
        finally:
            temporary.unlink(missing_ok=True)

    def configure(self, scheme: str, original: str, mode: str) -> list[str]:
        if mode != 'extreme':
            raise ValueError('只有極致節能可配置暫時方案。')
        warnings = []
        for index, supply in enumerate(('AC', 'DC')):
            self.api.write(scheme, *SLEEP, supply, 0)
            value = self.api.read(original, *DISPLAY, supply)
            maximum = (300, 120)[index]
            self.api.write(scheme, *DISPLAY, supply, min(value, maximum) if value else maximum)
            for setting, values in ((EPP, (90, 100)),
                                    (MAXIMUM, (80, 60)),
                                    (BOOST, (0, 0))):
                try:
                    self.api.read(original, *setting, supply)
                except PowerError as exc:
                    if exc.code != 'POWER_CAPABILITY_UNAVAILABLE':
                        raise
                    warnings.append('POWER_CAPABILITY_UNAVAILABLE:' + setting[1] + ':' + supply)
                    continue
                self.api.write(scheme, *setting, supply, values[index])
        return warnings

    def apply(self, mode: str, *, manage_power_scheme: bool = True,
              manage_windows_power_mode: bool = True) -> list[str]:
        PowerMode(mode)
        if mode == 'off':
            self.restore()
            return []
        # Finish an earlier transaction before taking a new snapshot.
        if self.state:
            self.restore()
        elif self.path.exists() or self.path.is_symlink():
            self.recover()
        warnings = []
        original = self.api.get_active()
        supported = False
        originals = {'AC': None, 'DC': None}
        if manage_windows_power_mode:
            if not self.api.supports_user_power_mode():
                warnings.append('POWER_USER_MODE_UNAVAILABLE')
            else:
                try:
                    originals = {supply: self.api.get_user_power_mode(supply)
                                 for supply in ('AC', 'DC')}
                    supported = True
                except PowerError:
                    warnings.append('POWER_USER_MODE_READ_FAILED')
                    originals = {'AC': None, 'DC': None}
        identity = self.api.process_identity(self.api.current())
        candidate = self.api.copy_scheme(original) if manage_power_scheme else None
        self.state = {
            'journal_version': 2, 'owner_pid': os.getpid(), 'owner_created': identity,
            'original': original, 'owned': [candidate] if candidate else [], 'mode': mode,
            'user_power_mode_supported': supported,
            'original_ac_power_mode': originals['AC'], 'original_dc_power_mode': originals['DC'],
            'applied_power_mode': GUID_POWER_MODE_BEST_EFFICIENCY,
        }
        try:
            self.journal()  # Durable intent precedes activation and both user-mode writes.
            if candidate:
                warnings.extend(self.configure(candidate, original, mode))
                self.api.set_active(candidate)
        except Exception as error:
            try:
                self.restore()
            except Exception:
                raise PowerError('POWER_ROLLBACK_FAILED') from error
            raise
        if supported:
            for supply in ('AC', 'DC'):
                try:
                    self.api.set_user_power_mode(supply, GUID_POWER_MODE_BEST_EFFICIENCY)
                except PowerError:
                    warnings.append('POWER_USER_MODE_APPLY_FAILED:' + supply)
        return warnings

