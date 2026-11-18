"""Exercise Pi's actual model/tool loop using one local scripted completions provider."""

import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread

cli = Path(sys.argv[1]).resolve(strict=True)
adapter = cli.parent / "integrations/pi/pi-layerfs"
calls = [
    (
        "bash",
        {
            "command": 'mkfifo /tmp/watch; (read x </tmp/watch && cat "a b" >/tmp/seen) </dev/null >/dev/null 2>&1 & pwd'
        },
    ),
    ("write", {"path": "a b", "content": "before\n"}),
    ("read", {"path": "a b"}),
    ("edit", {"path": "a b", "edits": [{"oldText": "before", "newText": "after"}]}),
    (
        "bash",
        {
            "command": "echo go >/tmp/watch; for i in {1..100}; do test -s /tmp/seen && break; sleep .01; done; cat /tmp/seen"
        },
    ),
    ("bash", {"command": "sleep 20", "timeout": 1}),
    ("read", {"path": "/workspace/a b"}),
    ("read", {"path": "many", "offset": 12000, "limit": 1}),
    ("read", {"path": "pixel"}),
]


class Model(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.requests.append(body)
        n = len(self.server.requests) - 1
        delta = {"role": "assistant"}
        if n < len(calls):
            name, args = calls[n]
            delta["tool_calls"] = [
                {
                    "index": 0,
                    "id": f"tool{n}",
                    "type": "function",
                    "function": {"name": name, "arguments": json.dumps(args)},
                }
            ]
        else:
            delta["content"] = "complete"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for part, reason in (
            (delta, None),
            ({}, "tool_calls" if n < len(calls) else "stop"),
        ):
            data = {
                "id": "test",
                "object": "chat.completion.chunk",
                "model": "test",
                "choices": [{"index": 0, "delta": part, "finish_reason": reason}],
            }
            self.wfile.write(("data: " + json.dumps(data) + "\n\n").encode())
        self.wfile.write(b"data: [DONE]\n\n")


def verify(tmp, model, terminal):
    disk = tmp / (terminal + ".ext4")
    subprocess.run(
        [cli, "init", disk, tmp / "source"], check=True, capture_output=True, timeout=10
    )

    def commits():
        raw = subprocess.check_output(
            [cli, "call", disk, "branch.list_commits", '{"branch":"work"}'], timeout=5
        )
        return [item["id"] for item in json.loads(raw)["items"]]

    before = commits()
    env = dict(
        os.environ,
        XDG_STATE_HOME=str(tmp / "state"),
        PI_CODING_AGENT_DIR=str(tmp / "config"),
    )
    model.requests = []
    with subprocess.Popen(
        [adapter, disk, "--no-session", "--mode", "rpc", "--model", "fixture/test"],
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    ) as pi:

        def prompt(message):
            pi.stdin.write(
                json.dumps({"id": message, "type": "prompt", "message": message}) + "\n"
            )
            pi.stdin.flush()

        prompt("exercise tools")
        results = []
        for line in pi.stdout:
            event = json.loads(line)
            if event["type"] == "tool_execution_end":
                results.append(
                    (event["toolName"], event["isError"], event["result"]["content"])
                )
            if event["type"] == "agent_end":
                break
        try:
            assert len(model.requests) == len(calls) + 1, (
                len(model.requests),
                results,
                event,
            )
            first = model.requests[0]
            assert {t["function"]["name"] for t in first["tools"]} == {
                "read",
                "bash",
                "edit",
                "write",
            }
            assert "fixture-project-rule" in str(
                first["messages"]
            ) and "/workspace" in str(first["messages"])
            assert [r[0] for r in results] == [name for name, _ in calls], results
            assert [i for i, result in enumerate(results) if result[1]] == [5], results
            for idx, token in [
                (0, "/workspace"),
                (2, "before"),
                (4, "after"),
                (5, "timed out"),
                (6, "after"),
                (7, "line-12000"),
            ]:
                assert token in str(results[idx][2]).lower(), results[idx]
            assert any(
                block["type"] == "image" and block["mimeType"] == "image/png"
                for block in results[8][2]
            ), results[8]
            if terminal == "commit":
                prompt("/layerfs-commit")
                for line in pi.stdout:
                    event = json.loads(line)
                    if event.get("id") == "/layerfs-commit":
                        assert event["success"], event
                        break
        finally:
            pi.stdin.close()
            assert pi.wait(timeout=8) == 0
    after = commits()
    assert (after[1:] if terminal == "commit" else after) == before, (before, after)
    model.requests = []
    check = subprocess.run(
        [
            cli,
            "run",
            disk,
            "--",
            "sh",
            "-ec",
            "test \"$(cat 'a b')\" = after"
            if terminal == "commit"
            else "test ! -e 'a b'",
        ],
        capture_output=True,
        timeout=10,
    )
    assert check.returncode == 0, (check.returncode, check.stderr)
    assert not (tmp / "source" / "a b").exists()


with (
    tempfile.TemporaryDirectory() as folder,
    ThreadingHTTPServer(("127.0.0.1", 0), Model) as server,
):
    tmp = Path(folder)
    (tmp / "source").mkdir()
    (tmp / "source" / "pixel").write_bytes(
        base64.b64decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC"
        )
    )
    (tmp / "source" / "many").write_text(
        "".join(f"line-{i}\n" for i in range(1, 12002))
    )
    (tmp / "source" / "AGENTS.md").write_text("fixture-project-rule")
    (tmp / "config").mkdir()
    provider = {
        "baseUrl": f"http://127.0.0.1:{server.server_port}/v1",
        "api": "openai-completions",
        "apiKey": "test",
        "models": [{"id": "test", "input": ["text", "image"]}],
    }
    (tmp / "config" / "models.json").write_text(
        json.dumps({"providers": {"fixture": provider}})
    )
    Thread(target=server.serve_forever, daemon=True).start()
    for terminal in ("commit", "discard"):
        verify(tmp, server, terminal)
        print(
            f"pi default tools, cancellation, background process and {terminal}: OK",
            flush=True,
        )
    server.shutdown()
