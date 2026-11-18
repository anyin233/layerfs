"""Exercise the unpacked release through the CLI as a regular host user."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from session import check_session


def smoke(cli, directory):
    disk = directory / "project, with spaces.ext4"
    source = directory / "input"
    source.mkdir()
    (source / "note").write_text("original\n")
    (source / "link").symlink_to("note")

    def invoke(*args, data=b"", code=0):
        result = subprocess.run(
            [cli, *args], input=data, capture_output=True, timeout=20, check=False
        )
        if result.returncode != code:
            raise AssertionError(
                (args, result.returncode, result.stdout, result.stderr)
            )
        return result

    def run(script, *args, branch="work", **kwargs):
        return invoke(
            "run",
            "--branch",
            branch,
            disk,
            "--",
            "/bin/bash",
            "-ec",
            script,
            "bash",
            *args,
            **kwargs,
        )

    def call(method, **args):
        return json.loads(invoke("call", disk, method, json.dumps(args)).stdout)

    invoke("init", disk, source)
    assert {b["name"] for b in call("project.list_branches")["items"]} == {
        "main",
        "work",
    }
    result = run(
        'test "$(id -u)" = 1000; test "$(cat link)" = original; '
        "cat > note; mkdir dir; touch dir/old; mv dir/old dir/new; chmod 700 dir/new; "
        'echo obsolete > removed; rm removed; printf "%s" "$1"; echo diagnostic >&2',
        "two words\nnext line",
        data=b"saved\n",
    )
    assert result.stdout == b"two words\nnext line" and result.stderr == b"diagnostic\n"
    original_head = call("branch.list_commits", branch="work")["items"][0]["id"]
    run("echo discarded > note; exit 23", code=23)
    assert call("branch.list_commits", branch="work")["items"][0]["id"] == original_head
    run('test "$(cat note)" = saved; test -x dir/new; test ! -e removed')
    call("branch.fork", name="feature", **{"from": "work"})
    run("echo side > feature", branch="feature")
    run("echo trunk > trunk")
    merged = call("branch.merge", branch="work", **{"with": "feature"}, name="combined")
    assert merged["kind"] != "Conflict", merged
    run('test "$(cat trunk)" = trunk; test "$(cat feature)" = side', branch="combined")
    call("branch.fork", name="restored", **{"from": original_head})
    run(
        'test "$(cat note)" = saved; test ! -e trunk; test ! -e feature',
        branch="restored",
    )
    call("fs.write", branch="restored", path="/api.txt", content="SDK write")
    assert call("fs.read", branch="restored", path="/api.txt")["text"] == "SDK write"
    call("branch.publish", branch="restored")
    assert call("fs.read", branch="main", path="/api.txt")["text"] == "SDK write"
    refused = invoke(
        "call", disk, "branch.list_commits", '{"branch":"missing"}', code=1
    )
    assert json.loads(refused.stdout)["refused"]["code"] == "NoSuchBranch"
    check_session(cli, disk)
    assert (source / "note").read_bytes() == b"original\n"
    assert set(os.listdir(source)) == {"note", "link"}


if __name__ == "__main__":
    if os.geteuid() == 0:
        sys.exit("Run the rootless smoke test under an ordinary user account.")
    with tempfile.TemporaryDirectory(prefix="layerfs-smoke-") as work:
        smoke(Path(sys.argv[1]).resolve(strict=True), Path(work))
    print("PASS: import, Workspace I/O, commit/discard, history, fork, merge, recovery")
