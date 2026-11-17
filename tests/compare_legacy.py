"""Optional differential acceptance against a read-only legacy checkout.

Only synthetic files and isolated application state are used. This is a test
driver; the Rust application never imports or launches the legacy runtime.
"""
import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import wave
import zipfile


class Peer:
    def __init__(self, command, root, local):
        env = os.environ.copy()
        for key in tuple(env):
            if key in {"CONTROL_PLANE_API_KEY", "MCP_LOCAL_RUST_SECURITY_CHANNEL",
                       "MCP_LOCAL_SECURITY_CHANNEL", "MCP_LOCAL_RUST_DEVELOPER_GUARD"}:
                env.pop(key)
        env.update(LOCALAPPDATA=str(local), USERPROFILE=str(local), PYTHONDONTWRITEBYTECODE="1")
        local.mkdir()
        self.process = subprocess.Popen(command + ["--root", str(root)], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                        env=env, creationflags=subprocess.CREATE_NO_WINDOW)
        self.queue = queue.Queue()
        self.sequence = 0
        def reader():
            for line in self.process.stdout:
                self.queue.put(json.loads(line))
        self.reader = threading.Thread(target=reader, daemon=True)
        self.reader.start()
        self.request("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                    "clientInfo": {"name": "parity-fixture", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, value):
        self.process.stdin.write(json.dumps(value, ensure_ascii=False).encode("utf-8") + b"\n")
        self.process.stdin.flush()

    def request(self, method, params):
        self.sequence += 1
        self.send({"jsonrpc": "2.0", "id": self.sequence, "method": method, "params": params})
        while True:
            response = self.queue.get(timeout=45)
            if response.get("id") == self.sequence:
                return response

    def call(self, name, arguments):
        result = self.request("tools/call", {"name": name, "arguments": arguments}).get("result", {})
        if result.get("isError"):
            return {"isError": True}
        data = result.get("structuredContent")
        if data is None:
            data = next((json.loads(item["text"]) for item in result.get("content", [])
                         if item.get("type") == "text" and item.get("text", "").startswith("{")), None)
        return {"isError": False, "value": data}

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.reader.join(timeout=2)


# These fields identify a particular run or native implementation, not data
# semantics. All other output fields are compared, including missing fields.
VOLATILE = {"elapsed_ms", "next_cursor", "snapshot_id", "snapshot_token", "baseline_token",
            "mcp_version", "parser_versions", "actual_contract_sha256", "expected_contract_sha256"}


def normalized(value):
    if isinstance(value, dict):
        return {k: normalized(v) for k, v in value.items() if k not in VOLATILE}
    if isinstance(value, list):
        return [normalized(v) for v in value]
    return value


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--legacy-python", type=Path, required=True)
    parser.add_argument("--legacy-entry", type=Path, required=True)
    parser.add_argument("--rust-exe", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    report = []
    with tempfile.TemporaryDirectory(prefix="mcp-differential-") as directory:
        fixture = Path(directory)
        root = fixture / "shared"
        project = root / "project"
        project.mkdir(parents=True)
        (project / "README.md").write_text("first line\nStraße 世界\nlast line\n", encoding="utf-8")
        (project / "Cargo.toml").write_text('[package]\nname="fixture"\nversion="0.1.0"\n', encoding="utf-8")
        (root / "sample.json").write_text('{"a":1,"b":["世界",true]}', encoding="utf-8")
        (root / "invalid.txt").write_bytes(b"valid first\ninvalid\xff")
        (root / ".hidden.txt").write_text("hidden", encoding="utf-8")
        (root / "image.ppm").write_bytes(b"P6\n1 1\n255\n\xff\0\0")
        with zipfile.ZipFile(root / "archive.zip", "w") as archive:
            archive.writestr("inside.txt", "fixture payload")
        with wave.open(str(root / "sound.wav"), "wb") as audio:
            audio.setparams((1, 2, 8000, 0, "NONE", "not compressed"))
            audio.writeframes(b"\0\0" * 800)
        cases = [
            ("list_directory", {}), ("list_files", {}),
            ("read_file", {"path": "project/README.md", "start_line": 2, "line_count": 1}),
            ("search_text", {"query": "STRASSE", "context_lines": 1}),
            ("workspace_info", {}), ("list_projects", {}), ("project_context", {"directory": "project"}),
            ("read_files", {"files": [{"path": "project/README.md"}, {"path": "sample.json"}]}),
            ("search_texts", {"queries": ["STRASSE", "世界"]}),
            ("find_files", {"queries": ["README", "json"]}),
            ("read_file_ranges", {"path": "project/README.md", "ranges": [{"start_line": 1, "line_count": 1}, {"start_line": 3, "line_count": 1}]}),
            ("hash_files", {"paths": ["sample.json"]}),
            ("compare_paths", {"left": "sample.json", "right": "project/README.md"}),
            ("project_status", {"directory": "project"}), ("server_diagnostics", {}),
            ("read_image", {"path": "image.ppm"}), ("file_info", {"path": "sample.json"}),
            ("read_document", {"path": "sample.json"}), ("inspect_media", {"path": "sound.wav"}),
            ("inspect_archive", {"path": "archive.zip"}), ("read_binary", {"path": "sample.json"}),
            ("query_files", {"extensions": ["json"], "include_sha256": True}),
            ("read_file", {"path": "../outside.txt"}), ("read_file", {"path": ".hidden.txt"}),
            ("read_file", {"path": "invalid.txt"}), ("read_file", {"path": "sample.json", "line_count": 401}),
        ]
        peers = []
        try:
            peers.append(Peer([str(args.legacy_python), "-B", str(args.legacy_entry)], root, fixture / "legacy-local"))
            peers.append(Peer([str(args.rust_exe)], root, fixture / "rust-local"))
            for name, arguments in cases:
                old, new = [peer.call(name, arguments) for peer in peers]
                old, new = normalized(old), normalized(new)
                row = {"tool": name, "arguments": arguments, "equal": old == new}
                if old != new:
                    row.update(legacy=old, rust=new)
                report.append(row)
        finally:
            for peer in reversed(peers):
                peer.close()
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({"cases": len(report), "equal": sum(row["equal"] for row in report),
                      "different": [row["tool"] for row in report if not row["equal"]]}))


if __name__ == "__main__":
    main()
