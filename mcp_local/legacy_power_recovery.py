"""One-time recovery of a legacy power transaction; never apply an active mode."""
from pathlib import Path


def recover_legacy_power(directory: Path) -> None:
    journal = directory / 'power-runtime.json'
    if not journal.exists() and not journal.is_symlink():
        return
    from mcp_local.power_policy import PowerRestoreGuardClient
    guard = PowerRestoreGuardClient(journal)
    try:
        guard.start()
        guard.close()
    except Exception:
        guard.abort()
        raise
    if journal.exists() or journal.is_symlink():
        raise RuntimeError('POWER_SCHEME_RESTORE_FAILED')
