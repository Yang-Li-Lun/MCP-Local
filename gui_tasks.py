"""單一 GUI 工作；工作完成僅送佇列，不接觸 Tk 元件。"""
import queue
import threading


class WakeQueue(queue.Queue):
    def __init__(self, notify=None):
        super().__init__()
        self.notify = notify

    def put(self, item, block=True, timeout=None):
        super().put(item, block, timeout)
        if self.notify:
            self.notify()


class GuiTasks:
    def __init__(self, notify=None):
        self.events = WakeQueue(notify)
        self.busy = False
        self.closed = False
        self.generation = 0

    def submit(self, work, complete):
        if self.busy or self.closed:
            return False
        self.busy = True
        self.generation += 1
        generation = self.generation
        def run():
            try:
                result, error = work(), None
            except Exception as exc:
                result, error = None, exc
            self.events.put((generation, complete, result, error))
        threading.Thread(target=run, name='mcp-gui-work', daemon=True).start()
        return True

    def poll(self):
        while not self.events.empty():
            generation, complete, result, error = self.events.get_nowait()
            if generation != self.generation:
                continue
            self.busy = False
            if not self.closed:
                complete(result, error)

    def close(self):
        self.closed = True
