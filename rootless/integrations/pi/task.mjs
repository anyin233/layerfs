// Serial ownership keeps aborts and replies paired with the command that caused them.
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

export class Task {
  constructor(cli, disk, branch) {
    this.child = spawn(cli, ["task", "--branch", branch, disk], {
      stdio: ["pipe", "pipe", "inherit"],
    });
    this.lines = createInterface({ input: this.child.stdout })[Symbol.asyncIterator]();
    this.closed = new Promise((resolve, reject) => {
      this.child.on("error", reject);
      this.child.on("close", (code, signal) => {
        if (code === 0) resolve();
        else reject(new Error(`Workspace closed with ${signal ?? code}. Inspect the branch before resuming.`));
      });
    });
    this.child.stdin.on("error", () => {}); // EOF and exit status also reach the reader.
    this.closed.catch(() => {});
    this.queue = this.next().then(reply => {
      if (!reply.ready) throw new Error("Expected Workspace readiness");
    });
    this.queue.catch(() => {});
  }

  async next() {
    const { value, done } = await this.lines.next();
    if (done) {
      await this.closed;
      throw new Error("Workspace closed before its reply");
    }
    return JSON.parse(value);
  }

  exec(argv, { stdin = "", signal, timeout, onData } = {}) {
    if (this.ending) throw new Error("Workspace is closing");
    const run = this.queue.then(async () => {
      if (signal?.aborted) throw new Error("aborted");
      this.child.stdin.write(JSON.stringify(["exec", { argv, stdin, stream: !!onData }]) + "\n");
      if (!(await this.next()).running) throw new Error("Expected command start");
      let reason;
      const cancel = message => {
        if (!reason) {
          reason = message;
          this.child.kill("SIGUSR1");
        }
      };
      const abort = () => cancel("aborted");
      signal?.addEventListener("abort", abort, { once: true });
      if (signal?.aborted) abort();
      const timer = timeout ? setTimeout(() => cancel(`timeout:${timeout}`), timeout * 1000) : undefined;
      try {
        for (;;) {
          const reply = await this.next();
          if ("data" in reply) {
            if (!reason) {
              try {
                await onData(Buffer.from(reply.data, "hex"));
              } catch (error) {
                cancel(error.message || "output consumer closed");
              }
            }
            continue;
          }
          if (reason) throw new Error(reason);
          const stdout = Buffer.from(reply.stdout.$bytes, "hex");
          const stderr = Buffer.from(reply.stderr.$bytes, "hex");
          if (onData && stderr.length) await onData(stderr);
          return { stdout, stderr, exitCode: reply.exit };
        }
      } finally {
        clearTimeout(timer);
        signal?.removeEventListener("abort", abort);
      }
    });
    this.queue = run.catch(() => {});
    return run;
  }

  finish(method) {
    if (!this.ending) {
      this.ending = this.queue.then(async () => {
        this.child.stdin.end(JSON.stringify([method, {}]) + "\n");
        if (!(await this.next()).running) throw new Error("Expected terminal request");
        await this.closed;
      });
    }
    return this.ending;
  }
}
