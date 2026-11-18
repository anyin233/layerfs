from pathlib import Path
import base64, hashlib, io, json, os, re, shlex, shutil, subprocess as sp, sys, tarfile

R = Path(os.environ["RUN_DIR"]).resolve(strict=True)
repo = Path(__file__).resolve().parents[3]
bundle = Path(os.environ["BUNDLE"]).resolve(strict=True)
cli = bundle / "layerfs-rootless"
launcher = bundle / "integrations/pi/pi-layerfs"


def cmd(*a, timeout=15, **kw):
    return sp.run(a, check=True, timeout=timeout, stdout=sp.PIPE, **kw).stdout


def git(*a):
    return cmd("git", "-C", repo, *a)


def du(p):
    return int(cmd("du", "-s", "-B1", "--", p).split()[0])


def sha(p):
    return hashlib.sha256(p.read_bytes()).hexdigest()


def dump(p, x):
    p.write_text(json.dumps(x, indent=2) + "\n")


states = (
    git("log", "--format=%H", "--first-parent", "-12", "40247720f", "--", "core/crates")
    .decode()
    .split()[::-1]
)
gen = R / "generate"
gen_install = (
    "printf %s "
    + base64.b64encode(gen.read_bytes()).decode()
    + " | base64 -d >/tmp/storage-gen; chmod 700 /tmp/storage-gen"
)


def q(p):
    return shlex.quote(str(p))


def listing(ref):
    return {
        p.decode(): (meta.split()[0], meta.split()[2])
        for meta, p in (
            row.split(b"\t", 1)
            for row in git("ls-tree", "-rz", ref + ":core/crates").split(b"\0")
            if row
        )
    }


def scratch_paths(root):
    return {str(p.relative_to(root)) for p in root.rglob("*") if p.is_dir()}


def check_guest_command(p, words):
    return 'test "$(sha256sum ' + q(p) + " | cut -d' ' -f1)\" = " + q(words)


def tree_digest(src):
    h = hashlib.sha256()
    for f in sorted(
        (p for p in src.rglob("*") if p.is_file()),
        key=lambda p: p.relative_to(src).as_posix().encode(),
    ):
        h.update((sha(f) + "  ./" + f.relative_to(src).as_posix() + "\n").encode())
    return h.hexdigest()


def body_of(disk, key):
    text = cmd(
        "debugfs", "-R", "stat /layerfs-guest/store.sqlite", disk, stderr=sp.DEVNULL
    ).decode()
    return int(re.search(key, text).group(1))


def measure(disk):
    st = disk.stat()
    return {
        "image_bytes": st.st_blocks * 512,
        "image_apparent_bytes": st.st_size,
        "store_bytes": body_of(disk, r"\bSize:\s*(\d+)"),
        "store_allocated_bytes": 512 * body_of(disk, r"Blockcount:\s*(\d+)"),
    }


def pi_commit(disk, commands):
    env = dict(
        os.environ,
        PI_CODING_AGENT_DIR=str(R / "config"),
        XDG_STATE_HOME=str(R / "state"),
        LAYERFS_CONSTRUCTION_WORKERS="1",
    )
    req = [
        {
            "id": str(i),
            "type": "bash",
            "command": "set -e; " + c,
            "excludeFromContext": True,
        }
        for i, c in enumerate(commands)
    ]
    req.append({"id": "finish", "type": "prompt", "message": "/layerfs-commit"})
    # Keep the pipe open until the terminal operation replies.
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
        stdin=sp.PIPE,
        stdout=sp.PIPE,
        env=env,
        text=True,
    ) as pi:
        try:
            for call in req:
                print(json.dumps(call), file=pi.stdin, flush=True)
                for line in pi.stdout:
                    event = json.loads(line)
                    if event.get("id") != call["id"] or event["type"] != "response":
                        continue
                    assert event["success"], event
                    if call["type"] == "bash":
                        assert event["data"]["exitCode"] == 0, event
                    break
                else:
                    raise RuntimeError("Pi closed before " + call["id"])
        finally:
            pi.stdin.close()
        assert pi.wait(timeout=6) == 0


def refs(case, i, previous, previous_keys):
    dest = case / "full" / f"{i:02}"
    dest.mkdir()
    if case.name == "source":
        rev = states[i]
        arc = git("archive", rev + ":core/crates")
        cmd("tar", "-xf", "-", "-C", dest, input=arc)
        keys = listing(rev)
    else:
        rev = None
        data = dest / "payload"
        size = 4 << 20
        if case.name == "patch" and i:
            shutil.copyfile(previous / "payload", data)
            with data.open("r+b") as f:
                f.seek(i * 73 * 4096)
                f.write(cmd(gen, str(i), "4096"))
        else:
            data.write_bytes(cmd(gen, str(i), str(size)))
        keys = {"payload": ("100644", sha(data))}
    linked = case / "linked" / f"{i:02}"
    linked.mkdir()
    for d in scratch_paths(dest):
        (linked / d).mkdir(parents=True, exist_ok=True)
    changed = sorted(p for p in keys if previous_keys.get(p) != keys[p])
    for p in keys:
        if i and keys[p] == previous_keys.get(p):
            os.link(case / "linked" / f"{i - 1:02}" / p, linked / p)
        else:
            shutil.copy2(dest / p, linked / p)
    return dest, keys, changed, rev


def changes(case, i, prev, dest, keys, old, changed):
    if case.name != "source":
        if case.name == "patch":
            action = f"/tmp/storage-gen {i} 4096 >/tmp/block && dd if=/tmp/block of=payload bs=4096 seek={i * 73} conv=notrunc 2>/dev/null"
        else:
            action = f"/tmp/storage-gen {i} {4 << 20} >payload"
        return [gen_install + "; " + action]
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz", format=tarfile.GNU_FORMAT) as tar:
        for p in changed:
            tar.add(dest / p, arcname=p, recursive=False)
    payload = base64.b64encode(buf.getvalue()).decode()
    commands = [
        "printf %s "
        + payload[n : n + 60000]
        + " | base64 -d "
        + (">" if n == 0 else ">>")
        + "/tmp/replay.tgz"
        for n in range(0, len(payload), 60000)
    ]
    deleted = old.keys() - keys.keys()
    dirs = scratch_paths(prev) - scratch_paths(dest)
    work = ["rm -- " + q(p) for p in sorted(deleted)]
    work += [
        "rmdir -- " + q(p) for p in sorted(dirs, key=lambda p: -len(Path(p).parts))
    ]
    # BusyBox tar creates parents and restores the regular file modes.
    work += ["tar xzf /tmp/replay.tgz"]
    return commands + ["; ".join(work)]


def verify(case, rows):
    check = case / "verify.ext4"
    cmd("cp", "--sparse=always", "--reflink=never", case / "project.ext4", check)

    def call(method, params):
        return json.loads(
            cmd(cli, "call", check, method, json.dumps(params), timeout=9.5)
        )

    history = call("branch.list_commits", {"branch": "work", "pageSize": 100})
    assert not history["hasMore"] and len(history["items"]) == len(rows) - 1, history
    versions = list(reversed(history["items"]))
    refs = [versions[0]["base"]] + [c["id"] for c in versions]
    for idx, c in enumerate(versions):
        assert c["parent"] == (None if idx == 0 else versions[idx - 1]["id"]), c
    requests = []
    roots = []
    for idx, (ref, row) in enumerate(zip(refs, rows)):
        branch = f"proof-{idx:02}"
        requests.append(["branch.fork", {"name": branch, "from": ref}])
        command = "set -e; " + check_guest_command(row["witness"], row["sha256"])
        if idx == len(rows) - 1 and case.name == "source":
            digest = tree_digest(case / "full" / f"{idx:02}")
            command += (
                "; test \"$(find . -type f | LC_ALL=C sort | xargs sha256sum | sha256sum | cut -d' ' -f1)\" = "
                + digest
            )
        requests.append(
            ["workspace.run", {"branch": branch, "argv": ["sh", "-ec", command]}]
        )
        roots.append(ref)
    replies = (
        cmd(
            cli,
            "session",
            check,
            input="".join(json.dumps(r) + "\n" for r in requests).encode(),
            timeout=9.5,
        )
        .decode()
        .splitlines()
    )
    assert len(replies) == len(requests)
    for n, line in enumerate(replies):
        resp = json.loads(line)
        assert "refused" not in resp and (n % 2 == 0 or resp["exit"] == 0), resp
    dump(
        case / "verification.json",
        {
            "states": roots,
            "commits": versions,
            "all_witnesses": True,
            "final_full_source_digest": digest if case.name == "source" else None,
        },
    )


def campaign(name, count=12):
    case = R / name
    case.mkdir()
    (case / "full").mkdir()
    (case / "linked").mkdir()
    disk = case / "project.ext4"
    prev = None
    old = {}
    rows = []
    for i in range(count):
        dest, keys, changed, rev = refs(case, i, prev, old)
        if i == 0:
            cmd(cli, "init", disk, dest)
        else:
            pi_commit(disk, changes(case, i, prev, dest, keys, old, changed))
        point = measure(disk)
        witness = changed[-1]
        row = dict(
            case=name,
            state=i + 1,
            git=rev,
            witness=witness,
            sha256=sha(dest / witness),
            files=len(keys),
            modified_or_added=len(changed),
            removed=len(old.keys() - keys.keys()),
            full_bytes=du(case / "full"),
            linked_bytes=du(case / "linked"),
            original_bytes=du(case / "full" / "00"),
            **point,
        )
        rows.append(row)
        with (case / "measurements.jsonl").open("a") as f:
            print(json.dumps(row), file=f)
        print(name, i + 1, row["image_bytes"], row["full_bytes"], flush=True)
        prev = dest
        old = keys
    verify(case, rows)
    print(name, "verified", flush=True)


if __name__ == "__main__":
    if not (R / "config").exists():
        (R / "config").mkdir()
        dump(
            R / "config" / "models.json",
            {
                "providers": {
                    "fixture": {
                        "baseUrl": "http://127.0.0.1:9/v1",
                        "api": "openai-completions",
                        "apiKey": "test",
                        "models": [{"id": "check"}],
                    }
                }
            },
        )
    campaign(sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 12)
