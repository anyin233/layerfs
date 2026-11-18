import json, re, subprocess as sp
from storage import R, cli, launcher, du, cmd, measure, dump, os

bundle = cli.parent


def accounting(disk):
    raw = cmd("filefrag", "-v", "-b4096", disk).decode()
    meta = cmd("dumpe2fs", disk, stderr=sp.DEVNULL).decode()
    table_spans = [
        tuple(map(int, m)) for m in re.findall(r"Inode table at (\d+)-(\d+)", meta)
    ]
    ext = [
        tuple(map(int, m))
        for m in re.findall(r"^\s*\d+:\s*(\d+)\.\.\s*(\d+):", raw, re.M)
    ]
    groups = [s for s in meta.splitlines() if re.match("^Group [0-9]+:", s)]
    assert len(table_spans) == 8 and len(groups) == 8 and ext
    allocated = (
        sum(
            max(0, min(a1, b1) - max(a0, b0) + 1)
            for a0, a1 in table_spans
            for b0, b1 in ext
        )
        * 4096
    )
    return dict(
        **measure(disk),
        itable_allocated_bytes=allocated,
        itable_zeroed_groups=sum("ITABLE_ZEROED" in x for x in groups),
        itable_groups=len(groups),
    )


def branch(disk):
    # Run observers on a separate closed byte copy; merely mounting for observation may allocate ext4 blocks.
    copy = disk.parent / "idle-observe.ext4"
    cmd("cp", "--sparse=always", "--reflink=never", disk, copy)
    return json.loads(
        cmd(
            cli,
            "call",
            copy,
            "branch.list_commits",
            '{"branch":"work","pageSize":100}',
            timeout=9.5,
        )
    )["items"]


def hold(disk):
    env = dict(
        os.environ,
        PI_CODING_AGENT_DIR=str(R / "config"),
        XDG_STATE_HOME=str(R / "state"),
        LAYERFS_CONSTRUCTION_WORKERS="1",
    )
    with sp.Popen(
        [
            "timeout",
            "15",
            launcher,
            disk,
            "--mode",
            "rpc",
            "--model",
            "fixture/check",
            "--no-session",
        ],
        env=env,
        stdin=sp.PIPE,
        stdout=sp.PIPE,
        text=True,
    ) as pi:
        for msg in [
            {
                "id": "open",
                "type": "bash",
                "command": "sleep 10 && pwd",
                "excludeFromContext": True,
            },
            {"id": "drop", "type": "prompt", "message": "/layerfs-discard"},
        ]:
            print(json.dumps(msg), file=pi.stdin, flush=True)
            for line in pi.stdout:
                event = json.loads(line)
                if event.get("id") == msg["id"] and event["type"] == "response":
                    assert event["success"] and (
                        msg["id"] == "drop"
                        or (
                            event["data"]["exitCode"] == 0
                            and "/workspace" in event["data"]["output"]
                        )
                    ), event
                    break
            else:
                raise RuntimeError("closed without " + msg["id"])
        pi.stdin.close()
        assert pi.wait(timeout=3) == 0


for case in ("source", "patch", "replace"):
    p = R / case
    disk = p / "project.ext4"
    historical = json.loads((p / "verification.json").read_text())["commits"]
    before = accounting(disk)
    dump(p / "pre-idle.json", before)
    hold(disk)
    after = accounting(disk)
    observed = list(reversed(branch(disk)))
    assert before["store_bytes"] == after["store_bytes"] and observed == historical, (
        before,
        after,
        [c["id"] for c in observed],
        [c["id"] for c in historical],
    )
    dump(
        p / "idle.json",
        dict(before=before, after=after, branch_history_unchanged=True, seconds=10),
    )
    print(case, before, after, flush=True)
