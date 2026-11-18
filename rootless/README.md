# Run LayerFS without host privileges

Download the Linux x86-64 runtime archive and `SHA256SUMS` from the [rootless preview](https://github.com/anyin233/layerfs/releases/tag/v1.1.0-rootless.1). Verify the runtime and extract both sparse disk images into a directory you own.

```sh
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf layerfs-rootless-linux-x86_64.tar.gz
cli="$(pwd)/layerfs-rootless-linux-x86_64/layerfs-rootless"
mkdir sample
printf 'before\n' > sample/example
"$cli" init project.ext4 sample
"$cli" run project.ext4 -- bash -ec 'cat example; echo after > example'
"$cli" run project.ext4 -- cat example
"$cli" call project.ext4 branch.list_commits '{}'
```

The host needs Linux x86-64, Python 3.7+, GNU `cp`, an executable bundle directory and a writable temporary directory that permits executable mappings. The host kernel and its security policy must allow the bundled UML kernel to boot in seccomp mode. The runtime includes UML, its loader, Bash, BusyBox and guest FUSE. Docker, sudo, a host FUSE device and KVM are unnecessary. A container administrator can still restrict required system calls. The guest has 512 MiB of configured memory and loopback networking. Only run code you trust.

## Project and command lifecycle

- `init DISK SOURCE` imports the directory once and creates a private sparse ext4 image with 1 GiB of logical capacity. Keep the image and the temporary directory outside the source. The source must be readable by your account.
- `run [--branch NAME] DISK -- COMMAND ARG...` boots the guest, mounts a fresh Workspace and runs the command there as guest UID 1000. The default Branch is `work`. The imported files keep their permission bits. A zero exit commits changes. A nonzero command exit discards that Workspace's changes and propagates the status. Subsequent invocations reopen the same Store. Edits affect the image only.
- Commands run in batch mode. Piped standard input is captured before boot. Standard output and error are returned after shutdown. Only Workspace runs load guest FUSE and bring up the loopback interface. The command must be available inside the guest or in the imported project with compatible Linux dependencies. There is no host-side Workspace mount or interactive terminal.
- One invocation may own a project image at a time. Keep the disk on a filesystem that supports advisory locks. Store history uses upstream's Disposable profile. Keep the original data or an independent backup. If the guest stops without confirming shutdown, inspect the retained disk before issuing another mutation.
- `call DISK METHOD JSON` accepts an upstream direct Store method and prints JSON. SDK refusals include the error code and use a nonzero exit status. A JSON object with a single `"$bytes"` field represents hex-encoded binary data. `main` names the published line of Layers. File calls and commit listing default to the writable `work` Branch.

## Continuous calls

A bundle built from this checkout supports `session DISK`. Keep its stdin and stdout open for the lifetime of a task. Send one UTF-8 JSON array per line, flush stdin, and read one JSON reply before sending the next request. The session owns the disk lock, one UML guest and one open Store shared by the direct methods and Workspace service until EOF. Closing stdin shuts down the guest. Check the process exit status to confirm shutdown.

```sh
"$cli" session project.ext4 <<'EOF'
["branch.fork",{"name":"session-work","from":"work"}]
["workspace.run",{"branch":"session-work","argv":["bash","-ec","cat > note"],"stdin":"saved in a workspace\n"}]
["fs.read",{"branch":"session-work","path":"/note"}]
EOF
```

Store calls use the same methods, defaults and replies as `call`. The first `workspace.run` prepares FUSE and the overlay template. Each run mounts a fresh Workspace and guest UID 1000. It has private process, mount, IPC, UTS and network namespaces, temporary directories and loopback. Commands share the guest kernel across requests. Supply stdin as text or `{"$bytes":"00ff"}`. The reply contains `exit`, `stdout` and `stderr`, with both streams encoded as hexadecimal `$bytes` objects. A successful command commits before its reply. A nonzero command discards its edits and ends the session with its exit status. Invalid input or a Store refusal also stops the session at that request. Earlier successful operations remain published. A disconnected client must inspect the project before resending an unconfirmed mutation.

## Pi coding agent

Use the bundle's Pi launcher on Linux x86-64 with Pi coding agent 1.5.2 on PATH.
Import the repository once, then open the project disk:

```sh
/path/to/bundle/layerfs-rootless init project.ext4 ./repository
/path/to/bundle/integrations/pi/pi-layerfs project.ext4
```

Pi runs on the host with its configured model and credentials. The model sees
its standard `read`, `edit`, `write` and `bash` tools and the path `/workspace`.
These tools and interactive `!` commands share one mounted Workspace and one guest
across turns. The project root's Pi instruction file comes from that Workspace.
A Bash process can leave a service running for later calls; redirect the service's
stdio to files. Shell variables belong to each Bash invocation. Supply project
executables and their Linux dependencies in the image or the project itself.
The included image provides Bash and BusyBox.

Enter `/layerfs-commit` to save the task to the selected Branch, or
`/layerfs-discard` to drop it. Either command closes the task and stops its
descendants. The next tool call opens a fresh Workspace. Normal Pi shutdown
discards an open task. In RPC mode send the slash command through `prompt` and
wait for its handled response before closing Pi. A print-only invocation with
no explicit commit also discards. Select a Branch with `LAYERFS_PI_BRANCH`
(default `work`). The original import directory is independent of later edits.

The launcher disables host project discovery, other Pi extensions, MCP and skills.
Pi UI features that read host paths directly, including host file attachments,
remain host operations. Additional host plugins need their own Workspace integration.

## Task protocol

`layerfs-rootless task [--branch NAME] DISK` owns the disk until the task
finishes. Wait for the JSON line `{"ready":true}`, then exchange requests
sequentially over stdin/stdout. Example requests:

```jsonl
["exec",{"argv":["bash","-c","cat > note"],"stdin":"pending change\n"}]
["exec",{"argv":["cat","note"],"stream":true}]
["commit",{}]
```

Each request starts with a `{"running":true}` acknowledgement. Streamed
commands emit combined output as `{"data":"<hex>"}` chunks. A completed exec
returns `exit`, `stdout` and `stderr` in the session Workspace format. Nonzero
exits leave pending changes available for the next tool. After acknowledgement,
SIGUSR1 cancels the foreground process group. The terminal requests are
`commit` and `discard`; wait for launcher exit zero to confirm the result.
Closing stdin without a terminal request discards. Inspect the Branch after a
lost terminal reply before attempting another mutation.

## Branches, merge and recovery

```sh
"$cli" call project.ext4 branch.fork '{"name":"experiment","from":"work"}'
"$cli" run --branch experiment project.ext4 -- bash -ec 'echo experiment > experiment.txt'
"$cli" call project.ext4 branch.merge '{"branch":"work","with":"experiment","name":"review"}'
"$cli" run --branch review project.ext4 -- cat experiment.txt
"$cli" call project.ext4 project.list_branches '{}'
```

To resume work at an earlier Commit, take its `id` from `branch.list_commits`, pass it as `from` to `branch.fork`, then use `run --branch` on the new Branch. Publishing a selected Branch to `main` is a separate, explicit `branch.publish` call. See the [public Store method table](../core/crates/layerfs-api/src/base/dispatch.rs) for file operations and history queries. The rootless entrypoint does not start the upstream Docker Sandbox or expose its JavaScript execution API.

## Build and verify

On a Linux x86-64 builder with Cargo and Rust, matching `rust-src`, a C/C++ toolchain, pkg-config, curl, Python 3, dpkg-deb, fakeroot, e2fsprogs, GNU tar and coreutils, run `rootless/build-bundle /absolute/path/to/new-output`. The builder downloads the locked Cargo dependencies and checksum-pinned Debian packages. The release includes corresponding component sources as a separate download. The launcher, guest kernel and project images relocate together.

Run the end-to-end check as an ordinary user from the checkout:

```sh
timeout 120s python3 rootless/tests/smoke.py "$cli"
```
