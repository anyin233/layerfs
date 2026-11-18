"""Live JSON-lines client, including mixed calls and failure boundaries."""

import json
import subprocess


def check_session(cli, disk):
    with subprocess.Popen(
        [cli, "session", disk], stdin=subprocess.PIPE, stdout=subprocess.PIPE
    ) as proc:

        def send(method, **args):
            proc.stdin.write(json.dumps([method, args]).encode() + b"\n")
            proc.stdin.flush()
            return json.loads(proc.stdout.readline())

        send("branch.fork", name="continuous", **{"from": "work"})
        busy = subprocess.run(
            [cli, "call", disk, "project.list_branches", "{}"],
            capture_output=True,
            timeout=5,
        )
        assert busy.returncode and b"another invocation owns" in busy.stderr
        binary = bytes(range(256)) * 400
        result = send(
            "workspace.run",
            branch="continuous",
            stdin={"$bytes": binary.hex()},
            argv=[
                "bash",
                "-ec",
                'test "$(id -u)" = 1000; cat; echo err >&2; '
                "touch /tmp/previous; echo kept > session-file; "
                "(cd / && exec sleep 30) </dev/null >/dev/null 2>&1 &",
            ],
        )
        assert result == {
            "exit": 0,
            "stdout": {"$bytes": binary.hex()},
            "stderr": {"$bytes": b"err\n".hex()},
        }
        assert (
            send("fs.read", branch="continuous", path="/session-file")["text"]
            == "kept\n"
        )
        before = send("branch.list_commits", branch="continuous")["items"][0]["id"]
        result = send(
            "workspace.run",
            branch="continuous",
            argv=[
                "bash",
                "-ec",
                "test ! -e /tmp/previous; test ! -r /request/argv; "
                "echo discard > session-file; exit 23",
            ],
        )
        assert result["exit"] == 23
        assert proc.wait(5) == 23

    def batch(*frames, code):
        output = subprocess.run(
            [cli, "session", disk],
            input=b"\n".join(frames) + b"\n",
            capture_output=True,
            timeout=10,
        )
        assert output.returncode == code, output
        return [json.loads(line) for line in output.stdout.splitlines()]

    result = batch(
        b'["branch.list_commits",{"branch":"continuous"}]',
        b'["fs.read",{"branch":"continuous","path":"/session-file"}]',
        code=0,
    )
    assert result[0]["items"][0]["id"] == before and result[1]["text"] == "kept\n"
    (refusal,) = batch(
        b'["branch.list_commits",{"branch":"missing"}]',
        b'["fs.write",{"path":"/late","content":"bad"}]',
        code=1,
    )
    assert refusal["refused"]["code"] == "NoSuchBranch"
    assert batch(b"malformed", code=1) == []
