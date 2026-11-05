"""本機記憶體限時授權；重開、重連、政策變更與通訊失效均撤銷。"""
from __future__ import annotations

import math
import time


class HostAuthorization:
    def __init__(self, clock=time.monotonic):
        self.clock = clock
        self.deadline = 0.0
        self.generation = 0
        self.reason = 'not_granted'

    def grant(self, minutes: int, maximum: int) -> None:
        if type(minutes) is not int or not 1 <= minutes <= maximum:
            raise ValueError('HOST_AUTH_DURATION：授權時間超過政策上限。')
        self.generation += 1
        self.deadline = self.clock() + minutes * 60
        self.reason = 'local_grant'

    def revoke(self, reason: str = 'local_revoke') -> None:
        self.generation += 1
        self.deadline = 0
        self.reason = reason

    def snapshot(self) -> dict:
        remaining = max(0, self.deadline - self.clock())
        return dict(generation=self.generation, deadline=self.deadline,
                    remaining_seconds=math.ceil(remaining), authorized=remaining > 0,
                    reason=self.reason if remaining or not self.deadline else 'expired')


def require_authorized(policy: dict, lease: dict, confirmed: bool, now: float | None = None) -> None:
    if policy['enabled'] and policy['host_authorization']:
        if not confirmed or lease.get('deadline', 0) <= (time.monotonic() if now is None else now):
            raise ValueError('HOST_AUTH_REQUIRED：請由本機安全中心限時授權；失效時拒絕命令與互動輸入。')
