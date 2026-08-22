import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";

const runtime = process.env.RUNLAB_TEST_RUNTIME_PATH ?? "/proc/1/root/bin/runc";
const runtimeFamily = process.env.RUNLAB_TEST_RUNTIME_FAMILY ?? "runc";
const workspace = fs.mkdtempSync(path.join(os.tmpdir(), "runlab-runtime-probe-"));
const runtimeRoot = path.join(workspace, "runtime");
const mountedRoots = new Set();
const outerCgroup = fs
  .readFileSync("/proc/self/cgroup", "utf8")
  .trim()
  .split("\n")
  .find((line) => line.startsWith("0::"))
  ?.slice(3);

if (!outerCgroup?.startsWith("/")) {
  throw new Error("the probe requires a cgroup v2 outer container");
}

fs.mkdirSync(runtimeRoot, { recursive: true });

function command(executable, args, options = {}) {
  const result = spawnSync(executable, args, {
    encoding: null,
    maxBuffer: 16 * 1024 * 1024,
    timeout: 5_000,
    ...options,
  });
  if (result.error) {
    throw result.error;
  }
  return result;
}

function requireSuccess(result, operation) {
  if (result.status !== 0) {
    throw new Error(
      `${operation} failed with ${result.status}: ${result.stderr.toString("utf8")}`,
    );
  }
}

function runtimeCommand(args, options = {}) {
  return command(runtime, ["--root", runtimeRoot, ...args], options);
}

function containerState(id) {
  const result = runtimeCommand(["state", id]);
  if (result.status !== 0) {
    return null;
  }
  return JSON.parse(result.stdout.toString("utf8"));
}

function waitForState(id, status, timeoutMs = 5_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const state = containerState(id);
    if (state?.status === status) {
      return state;
    }
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 10);
  }
  throw new Error(`${id} did not reach ${status} within ${timeoutMs}ms`);
}

function createBundle(id, args, memoryLimit = null, cgroupsPath = null) {
  const bundle = path.join(workspace, `bundle-${id}`);
  const rootfs = path.join(bundle, "rootfs");
  fs.mkdirSync(rootfs, { recursive: true });
  requireSuccess(command("mount", ["--bind", "/", rootfs]), `mount ${id}`);
  mountedRoots.add(rootfs);

  requireSuccess(runtimeCommand(["spec"], { cwd: bundle }), `spec ${id}`);
  const configPath = path.join(bundle, "config.json");
  const config = JSON.parse(fs.readFileSync(configPath, "utf8"));
  config.process.terminal = false;
  config.process.args = args;
  config.process.env = [
    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
  ];
  config.root.path = "rootfs";
  config.root.readonly = true;
  config.hostname = "runlab-probe";
  config.linux.cgroupsPath = cgroupsPath ?? (runtimeFamily === "youki"
    ? `/runlab-probe-${process.pid}-${id}`
    : `${outerCgroup}/runlab-probe-${id}`);
  if (memoryLimit !== null) {
    config.linux.resources.memory = { limit: memoryLimit, swap: memoryLimit };
    config.linux.namespaces = config.linux.namespaces.filter(
      (namespace) => namespace.type !== "cgroup",
    );
  }
  fs.writeFileSync(configPath, `${JSON.stringify(config)}\n`, { mode: 0o600 });
  return { bundle, rootfs };
}

function residualMounts(rootfs) {
  const prefix = `${rootfs}/`;
  return fs
    .readFileSync("/proc/self/mountinfo", "utf8")
    .trimEnd()
    .split("\n")
    .map((line) => line.split(" ")[4])
    .filter((mountpoint) => mountpoint === rootfs || mountpoint.startsWith(prefix));
}

function cleanupCase(id, rootfs) {
  runtimeCommand(["delete", "--force", id]);
  const mounts = residualMounts(rootfs);
  if (mounts.length !== 1 || mounts[0] !== rootfs) {
    throw new Error(`${id} retained runtime mounts: ${JSON.stringify(mounts)}`);
  }
  requireSuccess(command("umount", [rootfs]), `umount ${id}`);
  mountedRoots.delete(rootfs);
  if (residualMounts(rootfs).length !== 0) {
    throw new Error(`${id} retained its rootfs bind mount`);
  }
}

function stateIsAbsent(id) {
  if (containerState(id) !== null) {
    return false;
  }
  return !listedContainerIds().includes(id);
}

function listedContainerIds() {
  const args = runtimeFamily === "runc" ? ["list", "--format", "json"] : ["list"];
  const listed = runtimeCommand(args);
  requireSuccess(listed, "runtime list");
  if (runtimeFamily === "runc") {
    return (JSON.parse(listed.stdout.toString("utf8")) ?? []).map(
      (entry) => entry.id,
    );
  }
  return listed.stdout
    .toString("utf8")
    .trimEnd()
    .split("\n")
    .slice(1)
    .filter(Boolean)
    .map((line) => line.trimStart().split(/\s+/, 1)[0]);
}

function foregroundCase(id, args, input = Buffer.alloc(0), keep = false) {
  const { bundle, rootfs } = createBundle(id, args);
  const started = process.hrtime.bigint();
  try {
    const flags = ["run"];
    if (keep) {
      flags.push("--keep");
    }
    flags.push("--bundle", bundle, id);
    const result = runtimeCommand(flags, { input, timeout: 10_000 });
    return {
      status: result.status,
      signal: result.signal,
      stdoutHex: result.stdout.toString("hex"),
      stderrHex: result.stderr.toString("hex"),
      elapsedMs: Number((process.hrtime.bigint() - started) / 1_000_000n),
      stateAbsent: stateIsAbsent(id),
    };
  } finally {
    cleanupCase(id, rootfs);
  }
}

function collectChild(child, timeoutMs, id) {
  return new Promise((resolve, reject) => {
    const stdout = [];
    const stderr = [];
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Error(`${id} runtime client exceeded ${timeoutMs}ms`));
    }, timeoutMs);
    child.stdout.on("data", (chunk) => stdout.push(chunk));
    child.stderr.on("data", (chunk) => stderr.push(chunk));
    child.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.once("close", (status, signal) => {
      clearTimeout(timer);
      resolve({
        status,
        signal,
        stdout: Buffer.concat(stdout),
        stderr: Buffer.concat(stderr),
      });
    });
  });
}

function waitForBytes(stream, expected, timeoutMs, id) {
  return new Promise((resolve, reject) => {
    let observed = Buffer.alloc(0);
    const timer = setTimeout(
      () => reject(new Error(`${id} did not emit its readiness marker`)),
      timeoutMs,
    );
    function finish(error) {
      clearTimeout(timer);
      stream.off("data", onData);
      stream.off("close", onClose);
      if (error) {
        reject(error);
      } else {
        resolve();
      }
    }
    function onData(chunk) {
      observed = Buffer.concat([observed, chunk]);
      if (observed.includes(expected)) {
        finish();
      }
    }
    function onClose() {
      finish(new Error(`${id} exited before its readiness marker`));
    }
    stream.on("data", onData);
    stream.on("close", onClose);
  });
}

function processIds(id) {
  const result = runtimeCommand(["ps", "--format", "json", id]);
  requireSuccess(result, `ps ${id}`);
  return JSON.parse(result.stdout.toString("utf8"));
}

function processesAreGone(pids) {
  return pids.every((pid) => {
    try {
      process.kill(pid, 0);
      return false;
    } catch (error) {
      return error.code === "ESRCH";
    }
  });
}

async function controlledCase(id, args, trigger, triggerSignal, delayMs = 0) {
  const { bundle, rootfs } = createBundle(id, args);
  const child = spawn(
    runtime,
    ["--root", runtimeRoot, "run", "--bundle", bundle, id],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  child.stdin.end();
  const ready = waitForBytes(child.stdout, Buffer.from("ready\n"), 5_000, id);
  const completion = collectChild(child, 10_000, id);
  try {
    try {
      await ready;
    } catch (error) {
      const result = await completion;
      throw new Error(`${id} exited before readiness: ${JSON.stringify({
        readinessError: error.message,
        status: result.status,
        signal: result.signal,
        stdoutHex: result.stdout.toString("hex"),
        stderr: result.stderr.toString("utf8"),
      })}`);
    }
    const state = waitForState(id, "running");
    const observedCgroup = cgroupPath(state.pid);
    const pids = processIds(id);
    if (delayMs > 0) {
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, delayMs);
    }
    const killed = runtimeCommand(["kill", id, triggerSignal]);
    requireSuccess(killed, `kill ${id}`);
    const result = await completion;
    const absent = stateIsAbsent(id);
    return {
      trigger,
      triggerSignal,
      status: result.status,
      signal: result.signal,
      stdoutHex: result.stdout.toString("hex"),
      stderrHex: result.stderr.toString("hex"),
      observedPids: pids.length,
      processesGone: processesAreGone(pids),
      cgroupPath: observedCgroup,
      cgroupRemoved: !fs.existsSync(observedCgroup),
      stateAbsent: absent,
    };
  } finally {
    cleanupCase(id, rootfs);
  }
}

function cgroupPath(pid) {
  const unified = fs
    .readFileSync(`/proc/${pid}/cgroup`, "utf8")
    .trim()
    .split("\n")
    .find((line) => line.startsWith("0::"));
  if (!unified) {
    throw new Error(`no cgroup v2 path for pid ${pid}`);
  }
  return path.join("/sys/fs/cgroup", unified.slice(3));
}

function memoryEvents(cgroup) {
  return Object.fromEntries(
    fs
      .readFileSync(path.join(cgroup, "memory.events"), "utf8")
      .trim()
      .split("\n")
      .map((line) => {
        const [name, value] = line.split(" ");
        return [name, Number(value)];
      }),
  );
}

function cgroupPids(cgroup) {
  if (!fs.existsSync(cgroup)) {
    return [];
  }
  return fs
    .readFileSync(path.join(cgroup, "cgroup.procs"), "utf8")
    .trim()
    .split("\n")
    .filter(Boolean)
    .map(Number);
}

function removeResidualRootCgroup(cgroup) {
  if (!fs.existsSync(cgroup)) {
    return;
  }
  const killPath = path.join(cgroup, "cgroup.kill");
  if (fs.existsSync(killPath)) {
    fs.writeFileSync(killPath, "1");
  } else {
    for (const pid of cgroupPids(cgroup)) {
      try {
        process.kill(pid, "SIGKILL");
      } catch (error) {
        if (error.code !== "ESRCH") {
          throw error;
        }
      }
    }
  }
  const deadline = Date.now() + 5_000;
  while (fs.existsSync(cgroup) && cgroupPids(cgroup).length > 0 && Date.now() < deadline) {
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 10);
  }
  if (fs.existsSync(cgroup)) {
    requireSuccess(command("rmdir", [cgroup]), `remove residual cgroup ${cgroup}`);
  }
}

function rejectResidualRootCgroup(cgroup, phase) {
  if (!fs.existsSync(cgroup)) {
    return;
  }
  const pids = cgroupPids(cgroup);
  let cleanupError = null;
  try {
    removeResidualRootCgroup(cgroup);
  } catch (error) {
    cleanupError = error.message;
  }
  throw new Error(`${phase} root cgroup residue: ${JSON.stringify({
    cgroup,
    pids,
    cleanupError,
    removed: !fs.existsSync(cgroup),
  })}`);
}

async function oomCase() {
  const id = "oom";
  const rootCgroupName = runtimeFamily === "runc"
    ? `runlab-runc-oom-${process.pid}-${randomUUID()}`
    : null;
  const requestedCgroupsPath = rootCgroupName === null ? null : `/${rootCgroupName}`;
  const expectedRootCgroup = rootCgroupName === null
    ? null
    : path.join("/sys/fs/cgroup", rootCgroupName);
  if (expectedRootCgroup !== null) {
    rejectResidualRootCgroup(expectedRootCgroup, "preflight");
  }
  const args = [
    "/usr/local/bin/node",
    "-e",
    "const crypto=require('crypto');global.values=[];let total=0;process.stdout.write('ready\\n');setTimeout(()=>{const timer=setInterval(()=>{const value=Buffer.allocUnsafe(32*1024*1024);crypto.randomFillSync(value);global.values.push(value);total+=value.length;if(total>=1024*1024*1024){clearInterval(timer);process.stdout.write(`allocated:${total}\\n`)}},10)},500)",
  ];
  const { bundle, rootfs } = createBundle(
    id,
    args,
    192 * 1024 * 1024,
    requestedCgroupsPath,
  );
  const child = spawn(
    runtime,
    ["--root", runtimeRoot, "run", "--keep", "--bundle", bundle, id],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  child.stdin.end();
  const ready = waitForBytes(child.stdout, Buffer.from("ready\n"), 5_000, id);
  const completion = collectChild(child, 15_000, id);
  try {
    try {
      await ready;
    } catch (error) {
      const result = await completion;
      const stderr = result.stderr.toString("utf8");
      if (
        stderr.includes("unable to apply cgroup configuration") &&
        stderr.includes("domain threaded mode")
      ) {
        return {
          availability: "unavailable",
          reason: "LinuxKit nested cgroup cannot apply a memory controller",
          status: result.status,
          stderr,
          stateAbsent: stateIsAbsent(id),
        };
      }
      throw new Error(`OOM target exited before readiness: ${JSON.stringify({
        readinessError: error.message,
        status: result.status,
        signal: result.signal,
        stderr,
      })}`);
    }
    const state = waitForState(id, "running");
    const cgroup = cgroupPath(state.pid);
    if (expectedRootCgroup !== null && cgroup !== expectedRootCgroup) {
      throw new Error(`runc OOM target joined unexpected cgroup: ${JSON.stringify({
        expectedRootCgroup,
        observedCgroup: cgroup,
      })}`);
    }
    const observedPids = processIds(id);
    const observedPidCgroups = Object.fromEntries(
      observedPids.map((pid) => [pid, cgroupPath(pid)]),
    );
    const before = memoryEvents(cgroup);
    const memoryMax = fs.readFileSync(path.join(cgroup, "memory.max"), "utf8").trim();
    const memorySwapMax = fs
      .readFileSync(path.join(cgroup, "memory.swap.max"), "utf8")
      .trim();
    const allocation = waitForBytes(
      child.stdout,
      Buffer.from("allocated:"),
      5_000,
      id,
    ).then(
      () => "allocated",
      () => "exited",
    );
    const outcome = await Promise.race([
      completion.then(() => "exited"),
      allocation,
    ]);
    if (outcome === "allocated") {
      const observedMemoryCurrent = fs
        .readFileSync(path.join(cgroup, "memory.current"), "utf8")
        .trim();
      const observedMemorySwapCurrent = fs
        .readFileSync(path.join(cgroup, "memory.swap.current"), "utf8")
        .trim();
      const observedEvents = memoryEvents(cgroup);
      requireSuccess(runtimeCommand(["kill", id, "KILL"]), "kill OOM control case");
      const result = await completion;
      return {
        availability: "unavailable",
        reason: "Youki v0.7.0 left memory.swap.max unlimited after accepting swap=limit",
        status: result.status,
        signal: result.signal,
        stdoutHex: result.stdout.toString("hex"),
        stderrHex: result.stderr.toString("hex"),
        cgroupV2: true,
        requestedMemoryMax: 192 * 1024 * 1024,
        requestedMemorySwapMax: 192 * 1024 * 1024,
        observedMemoryMax: memoryMax,
        observedMemorySwapMax: memorySwapMax,
        observedMemoryCurrent,
        observedMemorySwapCurrent,
        observedOomKillDelta: observedEvents.oom_kill - before.oom_kill,
        observedPidCgroups,
        cgroupRemovedAfterControlKill: !fs.existsSync(cgroup),
        stateAbsent: stateIsAbsent(id),
      };
    }
    const result = await completion;
    if (!fs.existsSync(cgroup)) {
      return {
        availability: "unavailable",
        reason: "foreground runtime removed the cgroup before OOM facts could be read",
        status: result.status,
        signal: result.signal,
        stdoutHex: result.stdout.toString("hex"),
        stderrHex: result.stderr.toString("hex"),
        cgroupV2: true,
        cgroupRemovedBeforeObservation: true,
        stateAbsent: stateIsAbsent(id),
      };
    }
    const after = memoryEvents(cgroup);
    const retained = containerState(id)?.status === "stopped";
    const delta = after.oom_kill - before.oom_kill;
    runtimeCommand(["delete", "--force", id]);
    return {
      availability: "available",
      status: result.status,
      signal: result.signal,
      stdoutHex: result.stdout.toString("hex"),
      stderrHex: result.stderr.toString("hex"),
      cgroupV2: true,
      cgroupPath: cgroup,
      requestedMemoryMax: 192 * 1024 * 1024,
      requestedMemorySwapMax: 192 * 1024 * 1024,
      observedMemoryMax: memoryMax,
      observedMemorySwapMax: memorySwapMax,
      oomKillDelta: delta,
      stateRetainedUntilDelete: retained,
      cgroupRemovedAfterDelete: !fs.existsSync(cgroup),
      stateAbsent: stateIsAbsent(id),
    };
  } finally {
    let cleanupError = null;
    try {
      cleanupCase(id, rootfs);
    } catch (error) {
      cleanupError = error;
    }
    if (expectedRootCgroup !== null) {
      try {
        rejectResidualRootCgroup(expectedRootCgroup, "post-cleanup");
      } catch (error) {
        if (cleanupError !== null) {
          throw new AggregateError([cleanupError, error], "OOM cleanup failed");
        }
        throw error;
      }
    }
    if (cleanupError !== null) {
      throw cleanupError;
    }
  }
}

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

try {
  const versionResult = command(runtime, ["--version"]);
  requireSuccess(versionResult, "runtime version");
  const version = versionResult.stdout.toString("utf8").trim();
  const stdin = Buffer.from([0x00, 0x41, 0xff, 0x0a]);
  const exact = foregroundCase("exact", [
    "/usr/local/bin/node",
    "-e",
    "const fs=require('fs');const b=fs.readFileSync(0);process.stdout.write(Buffer.concat([Buffer.from([0,111,117,116,58]),b]));process.stderr.write(Buffer.concat([Buffer.from([0,101,114,114,58]),b,Buffer.from([255])]))",
  ], stdin);
  const exitSeven = foregroundCase("exit-seven", [
    "/usr/local/bin/node",
    "-e",
    "process.stdout.write('seven-out');process.stderr.write('seven-err');process.exit(7)",
  ]);
  const fastExit = foregroundCase("fast-exit", ["/bin/true"]);
  const selfSignal = foregroundCase("self-signal", [
    "/usr/local/bin/node",
    "-e",
    "process.abort()",
  ]);
  selfSignal.targetAction = "process.abort";
  let cancelled;
  try {
    cancelled = await controlledCase("cancel", [
      "/usr/local/bin/node",
      "-e",
      "process.on('SIGTERM',()=>{process.stderr.write('cancelled');process.exit(42)});require('child_process').spawn(process.execPath,['-e','setInterval(()=>{},1000)']);process.stdout.write('ready\\n');setInterval(()=>{},1000)",
    ], "cancel", "TERM");
  } catch (error) {
    throw new Error(`${error.message}; foreground prerequisites: ${JSON.stringify({
      exact,
      exitSeven,
      fastExit,
      selfSignal,
    })}`);
  }
  const timedOut = await controlledCase("timeout", [
    "/usr/local/bin/node",
    "-e",
    "require('child_process').spawn(process.execPath,['-e','setInterval(()=>{},1000)']);process.stdout.write('ready\\n');setInterval(()=>{},1000)",
  ], "deadline", "KILL", 200);
  const oom = await oomCase();

  assert(exact.status === 0, "exact stream case did not exit 0");
  assert(exact.stdoutHex === `006f75743a${stdin.toString("hex")}`, "stdout bytes changed");
  assert(exact.stderrHex === `006572723a${stdin.toString("hex")}ff`, "stderr bytes changed");
  assert(exitSeven.status === 7, "non-zero target exit was not preserved");
  assert(fastExit.status === 0 && fastExit.stateAbsent, "fast exit retained runtime state");
  const selfSignalStatus = runtimeFamily === "youki" ? 5 : 133;
  assert(
    selfSignal.status === selfSignalStatus && selfSignal.stateAbsent,
    `self signal behavior changed: ${JSON.stringify(selfSignal)}`,
  );
  assert(cancelled.status === 42, "graceful cancellation exit changed");
  assert(cancelled.stderrHex === Buffer.from("cancelled").toString("hex"), "cancel stderr changed");
  assert(cancelled.observedPids >= 2 && cancelled.processesGone, "cancel leaked a process tree");
  assert(cancelled.cgroupRemoved && cancelled.stateAbsent, "cancel retained runtime state");
  const timeoutStatus = runtimeFamily === "youki" ? 9 : 137;
  assert(timedOut.status === timeoutStatus, "timeout kill exit changed");
  assert(timedOut.observedPids >= 2 && timedOut.processesGone, "timeout leaked a process tree");
  assert(timedOut.cgroupRemoved && timedOut.stateAbsent, "timeout retained runtime state");
  if (oom.availability === "available") {
    assert(oom.status === 137 && oom.oomKillDelta > 0, "OOM is not proven by memory.events");
    assert(oom.stateRetainedUntilDelete, "--keep did not retain stopped OOM state");
    assert(oom.cgroupRemovedAfterDelete && oom.stateAbsent, "OOM cleanup retained state");
  } else {
    assert(oom.availability === "unavailable" && oom.stateAbsent, "OOM probe failed ambiguously");
  }

  assert(listedContainerIds().length === 0, "runtime root is not empty");
  process.stdout.write(`${JSON.stringify({
    runtime: version,
    kernel: os.release(),
    cgroup: fs.readFileSync("/proc/self/cgroup", "utf8").trim(),
    cases: { exact, exitSeven, fastExit, selfSignal, cancelled, timedOut, oom },
  })}\n`);
} finally {
  for (const rootfs of mountedRoots) {
    command("umount", ["-l", rootfs]);
  }
  fs.rmSync(workspace, { recursive: true, force: true });
}
