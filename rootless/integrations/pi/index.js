import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  createBashToolDefinition, createEditToolDefinition, createReadToolDefinition,
  createWriteToolDefinition, detectSupportedImageMimeTypeFromFile,
} from "@earendil-works/pi-coding-agent";
import { Task } from "./task.mjs";

const cwd = "/workspace";

export default function (pi) {
  let task;
  function workspace() {
    return task ??= new Task(process.env.LAYERFS_ROOTLESS_CLI, process.env.LAYERFS_PI_DISK,
      process.env.LAYERFS_PI_BRANCH || "work");
  }
  async function file(argv, stdin = "", signal) {
    const { exitCode, stdout, stderr } = await workspace().exec(argv, { stdin, signal });
    if (exitCode) throw new Error(stderr.toString().trim() || `${argv.join(" ")} exited ${exitCode}`);
    return stdout;
  }
  const bash = {
    exec(command, _cwd, options) {
      return workspace().exec(["/bin/bash", "-c", command], options);
    },
  };
  function readOps(signal) {
    // Pi checks access, sniffs the header, then reads. Keep only this call's prefix.
    let prefix;
    return {
      async access(path) {
        prefix = await file(["head", "-c", "4101", "--", path], "", signal);
      },
      readFile(path) {
        return prefix.length <= 4100 ? prefix : file(["cat", "--", path], "", signal);
      },
      async *createReadStream(path) {
        if (prefix.length <= 4100) {
          yield prefix;
          return;
        }
        const { readable, writable } = new TransformStream();
        const writer = writable.getWriter();
        workspace().exec(["cat", "--", path], { signal, onData: data => writer.write(data) })
          .then(({ exitCode }) => {
            if (exitCode) throw new Error(`cat ${path} exited ${exitCode}`);
            return writer.close();
          }).catch(error => writer.abort(error));
        yield* readable;
      },
      async detectImageMimeType() {
        if (![0xff, 0x89, 0x47, 0x52, 0x42].includes(prefix[0])) return null;
        const dir = await mkdtemp(join(tmpdir(), "pi-image-"));
        try {
          const name = join(dir, "header");
          await writeFile(name, prefix.subarray(0, 4100));
          return await detectSupportedImageMimeTypeFromFile(name);
        } finally {
          await rm(dir, { recursive: true });
        }
      },
    };
  }

  const guestWrite = (path, content, signal) =>
    file(["sh", "-c", 'cat > "$1"', "sh", path], content, signal);
  const definitions = [
    signal => createReadToolDefinition(cwd, { operations: readOps(signal) }),
    signal => createWriteToolDefinition(cwd, { operations: {
      mkdir: path => file(["mkdir", "-p", "--", path], "", signal),
      writeFile: (path, content) => guestWrite(path, content, signal),
    } }),
    signal => createEditToolDefinition(cwd, { operations: {
      access: path => file(["sh", "-c", 'test -r "$1" && test -w "$1"', "sh", path], "", signal),
      readFile: path => file(["cat", "--", path], "", signal),
      writeFile: (path, content) => guestWrite(path, content, signal),
    } }),
    () => createBashToolDefinition(cwd, { operations: bash }),
  ];
  for (const define of definitions) {
    const definition = define();
    pi.registerTool({ ...definition, execute(id, params, signal, update, ctx) {
      const name = params.path?.replace(/^@/, "");
      if (name === "~" || name?.startsWith("~/")) {
        params = { ...params, path: "/tmp" + name.slice(1) };
      }
      return define(signal).execute(id, params, signal, update, { ...ctx, cwd });
    } });
  }
  pi.on("user_bash", () => ({ operations: bash }));
  pi.on("before_agent_start", async event => {
    event.systemPromptOptions.cwd = cwd;
    const name = (await file(["sh", "-c",
      'for f in AGENTS.override.md AGENTS.md AGENTS.MD CLAUDE.md CLAUDE.MD; do if test -f "$f"; then printf "%s" "$f"; break; fi; done'])).toString();
    if (name) event.systemPromptOptions.contextFiles.push({ path: join(cwd, name), content: (await file(["cat", name])).toString() });
  });
  async function finish(method) {
    if (!task) return;
    await task.finish(method);
    task = undefined;
  }
  for (const method of ["commit", "discard"]) {
    pi.registerCommand(`layerfs-${method}`, { description: `${method} the current Workspace`,
      async handler(_args, ctx) {
        await finish(method);
        ctx.ui.notify(`Workspace closed (${method})`, "info");
      },
    });
  }
  pi.on("session_shutdown", () => finish("discard"));
}
