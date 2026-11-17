#!/usr/bin/env node
// Built-SPA journey for local-first private sessions, end to end on loopback:
// the REAL `conversations-server` binary serving the BUILT SPA (auth on, fresh
// SQLite), the REAL `arrakis-conversations` host binary (normal backend, fresh
// HOME/runtime/settings), and a REAL headless Chromium driven over CDP with a
// virtual WebAuthn authenticator. Only the coding agent is a recording stub.
//
// It never inherits the developer's environment: every child gets a minimal,
// explicit env (no CONVERSATIONS_*, CMUX_*, real HOME, agent config, or
// production endpoint). It owns every child it spawns and stops them through
// the ChildProcess handle or the stub's stop file; it never signals a PID read
// from a log.
//
// Usage (all paths absolute; ROOT must not exist yet):
//   PRIVATE_APP_CHROME=/path/to/chrome \
//   PRIVATE_APP_BIN_DIR=/target/debug \
//   PRIVATE_APP_DIST=/built/spa \
//   PRIVATE_APP_ROOT=/fresh/dir \
//   PRIVATE_APP_RUNTIME_PARENT="$(getconf DARWIN_USER_TEMP_DIR)" \
//   node tests/private-app-e2e.mjs
//
// Writes ROOT/evidence.json and exits 0 only if every step passed AND cleanup
// was clean (every child exited, every agent wrote its done marker, nothing
// still uses the runtime dir). Every regular file in the runtime dir is
// copied to ROOT/host-runtime; the dir is removed only after a passed run
// whose copy succeeded, and is otherwise left in place for inspection.

/* global fetch, WebSocket */

import { Buffer } from "node:buffer";
import { spawn, spawnSync } from "node:child_process";
import console from "node:console";
import crypto from "node:crypto";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import process from "node:process";
import { clearTimeout, setTimeout } from "node:timers";

const env = (name) => {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set (no fallback is used)`);
  return value;
};
const CHROME = env("PRIVATE_APP_CHROME");
const BIN_DIR = env("PRIVATE_APP_BIN_DIR");
const DIST = env("PRIVATE_APP_DIST");
const ROOT = env("PRIVATE_APP_ROOT");
const HOST_BIN = path.join(BIN_DIR, "arrakis-conversations");
const SERVER_BIN = path.join(BIN_DIR, "conversations-server");
const SQLITE3 = process.env.PRIVATE_APP_SQLITE3 ?? "/usr/bin/sqlite3";
const OVERALL_TIMEOUT_MS = Number(
  process.env.PRIVATE_APP_TIMEOUT_MS ?? 900_000,
);

// Markers: each appears in exactly one place, so finding it elsewhere is a leak.
const TRANSCRIPT_MARK = "transcriptmarkzebrawood";
const MUTATION_MARK = "mutationmarkbristlecone";
const AFTER_UNSHARE_MARK = "afterunsharemarkcorkoak";
const REPLY_MARK = "replymarkquillwort";
// The agent's screen, its reaction to a dropped file, and the transfers' bytes.
const SCREEN_MARK = "screenmarkhornbeam";
const UPDATE_MARK = "updatemarkbuckthorn";
const FILE_NAME = "dropmarkalder.txt";
const FILE_BODY_MARK = "filebodymarkhawthorn";
const IMAGE_MARK = "imagemarkblackthorn";
const LEASE_MARK = "leasemarkspindle";
const LEASE_AGAIN_MARK = "leaseagainmarkrowan";
const REVOKED_MARK = "revokedmarkelder";
// Opt-in real TURN: the coturn binary to run as an owned child. Unset keeps the
// default direct journey unchanged.
const TURN_BIN = process.env.PRIVATE_APP_TURN_BIN ?? "";
const TURN_USER = "turnusermarkjuniper";
const TURN_GOOD = "turncredmarkgoodsloe";
const TURN_BAD = "turncredmarkbadmedlar";
const BAD_REPLY_MARK = "badreplymarkyew";
// Longer than the 60 s signed terminal lease, so a paused page misses renewal.
const LEASE_LAPSE_MS = 75_000;
const PASSWORD = "loopback-only-password";
const OWNER = "owner@acme.test";
const READER = "reader@acme.test";
const OUTSIDER = "outsider@acme.test";

if (fs.existsSync(ROOT)) throw new Error(`${ROOT} exists; use a fresh root`);
for (const dir of [
  "",
  "central",
  "host",
  "host/work",
  "host/stubs",
  "host/tmp",
  "chrome",
  "logs",
]) {
  fs.mkdirSync(path.join(ROOT, dir), { recursive: true });
}
const STUBS = path.join(ROOT, "host/stubs");
const evidence = {
  steps: [],
  leaks: {},
  commands: [],
  started: new Date().toISOString(),
};
const step = (name, detail = {}) => {
  evidence.steps.push({ name, at: new Date().toISOString(), ...detail });
  console.log(`STEP ${name} ${JSON.stringify(detail)}`);
};

// ------------------------------------------------------------ process care --

// Test-only fault injection, to prove cleanup on failure paths:
//   after-startup  throw once central and the host are up
//   hang-cdp       never send Target.createTarget (the CDP reply never comes)
//   pgrep-fail     the cleanup's pgrep fails (an invalid option: exit 2)
const FAULT = process.env.PRIVATE_APP_FAULT ?? "";
const CDP_TIMEOUT_MS = Number(process.env.PRIVATE_APP_CDP_TIMEOUT_MS ?? 60_000);

const owned = new Set();
/**
 * Track a spawned child until it exits. `child.started` settles once the OS
 * started it or refused to (ENOENT, EACCES): a failed spawn is an error the
 * journey sees, never an unhandled event that skips cleanup.
 */
function own(child, name) {
  owned.add(child);
  child.name = name;
  child.exited = new Promise((resolve) => {
    child.once("exit", () => {
      owned.delete(child);
      resolve();
    });
    child.once("error", () => {
      // A child that never started emits no exit.
      if (child.pid === undefined) {
        owned.delete(child);
        resolve();
      }
    });
  });
  child.started = new Promise((resolve, reject) => {
    child.once("spawn", resolve);
    child.once("error", (error) =>
      reject(new Error(`${name} did not start: ${error.message}`)),
    );
  });
  child.started.catch(() => undefined);
  return child;
}

/**
 * Processes still naming this run's runtime dir (read only; never signalled).
 * Only pgrep's own answers count: 0 (matches) or 1 (none). A timeout, signal
 * or any other status throws, so an unknown answer never reads as "none".
 */
function runtimeUsers() {
  const args = FAULT === "pgrep-fail" ? ["--no-such-option"] : [];
  const out = spawnSync("/usr/bin/pgrep", [...args, "-f", RUNTIME], {
    encoding: "utf8",
    timeout: 5_000,
  });
  if (out.error || out.signal || (out.status !== 0 && out.status !== 1)) {
    throw new Error(
      `pgrep failed: status=${out.status} signal=${out.signal} error=${out.error?.message ?? ""} stderr=${String(out.stderr ?? "").trim()}`,
    );
  }
  return out.status === 1 ? 0 : out.stdout.split("\n").filter(Boolean).length;
}

/** Copy regular files (not sockets) from `from` into `to`, keeping the tree. */
function copyRegularFiles(from, to) {
  if (!fs.existsSync(from)) return;
  fs.mkdirSync(to, { recursive: true });
  for (const entry of fs.readdirSync(from, { withFileTypes: true })) {
    const source = path.join(from, entry.name);
    const target = path.join(to, entry.name);
    if (entry.isDirectory()) copyRegularFiles(source, target);
    else if (entry.isFile()) fs.copyFileSync(source, target);
  }
}

function listFiles(dir, prefix = "") {
  if (!fs.existsSync(dir)) return [];
  return fs
    .readdirSync(dir, { withFileTypes: true })
    .flatMap((entry) =>
      entry.isDirectory()
        ? listFiles(path.join(dir, entry.name), `${prefix}${entry.name}/`)
        : [`${prefix}${entry.name}`],
    );
}

let cleaning = null;
async function cleanup(passed) {
  if (cleaning) return cleaning;
  cleaning = (async () => {
    const report = {};
    // The stub agent exits on its own stop file; its wrapper follows it.
    fs.writeFileSync(path.join(STUBS, "stop"), "stop");
    // A child the journey stopped (SIGSTOP) is continued first, so it can exit.
    for (const child of owned) child.kill("SIGCONT");
    for (const child of [...owned].reverse()) child.kill("SIGTERM");
    const settle = (ms) =>
      Promise.race([Promise.all([...owned].map((c) => c.exited)), sleep(ms)]);
    await settle(8_000);
    report.killed = [...owned].map((c) => c.name);
    for (const child of owned) child.kill("SIGKILL");
    await settle(5_000);
    report.unexited = [...owned].map((c) => c.name);
    // Every agent this run started writes its done marker when it exits.
    const runs = fs.existsSync(path.join(STUBS, "claude.runs"))
      ? fs
          .readFileSync(path.join(STUBS, "claude.runs"), "utf8")
          .split("\n")
          .filter(Boolean)
          .map((l) => l.split(" ")[l.split(" ").indexOf("--session-id") + 1])
      : [];
    const undone = () =>
      [...new Set(runs)].filter(
        (s) => !fs.existsSync(path.join(STUBS, `claude.done.${s}`)),
      );
    const markers = Date.now() + 10_000;
    while (undone().length && Date.now() < markers) await sleep(100);
    report.agentsNotDone = undone();
    // Wrappers the host spawned exit after their agent; wait, never signal.
    // If pgrep cannot answer, the users are unknown: keep the runtime.
    try {
      const users = Date.now() + 10_000;
      while (runtimeUsers() && Date.now() < users) await sleep(200);
      report.runtimeUsers = runtimeUsers();
    } catch (error) {
      report.runtimeUsersError = String(error);
      report.runtimeUsers = "unknown";
    }
    // Keep everything the host wrote as evidence. The mkdtemp dir is removed
    // only after a passed run whose copy succeeded; a failed run keeps it.
    const kept = path.join(ROOT, "host-runtime");
    try {
      copyRegularFiles(RUNTIME, kept);
      report.keptRuntimeFiles = listFiles(kept);
    } catch (error) {
      report.copyError = String(error);
    }
    if (passed && !report.runtimeUsers && !report.copyError)
      fs.rmSync(RUNTIME, { recursive: true, force: true });
    report.runtimeRemoved = !fs.existsSync(RUNTIME);
    if (!report.runtimeRemoved) report.runtimeLeftAt = RUNTIME;
    report.clean =
      !report.unexited.length &&
      !report.agentsNotDone.length &&
      !report.runtimeUsers &&
      !report.copyError &&
      (report.runtimeRemoved || !passed);
    evidence.cleanup = report;
    return report;
  })();
  return cleaning;
}

/** Record the outcome after cleanup, so the evidence says whether cleanup was clean. */
async function finish(result, error) {
  if (error) {
    evidence.error = String(error?.stack ?? error);
    console.error(`FAIL ${error?.stack ?? error}`);
    // What each page showed when the run failed (bounded by the CDP timeout).
    evidence.pages = {};
    for (const page of pages) {
      const snapshot = { console: page.console.slice(-50) };
      try {
        await page.resume();
        snapshot.text = String(await page.text()).slice(0, 4000);
        snapshot.terminal = await page.eval(
          `document.querySelector('[data-testid="live-terminal-screen"]')?.outerHTML.slice(0, 4000) ?? null`,
        );
      } catch (e) {
        snapshot.unavailable = String(e);
      }
      evidence.pages[page.name] = snapshot;
    }
  }
  evidence.result ??= result;
  // A cleanup that throws is dirty, and the evidence is still written.
  let report;
  try {
    report = await cleanup(evidence.result === "passed");
  } catch (error) {
    report = { clean: false, cleanupError: String(error?.stack ?? error) };
    evidence.cleanup = { ...evidence.cleanup, ...report };
  }
  evidence.finished = new Date().toISOString();
  fs.writeFileSync(
    path.join(ROOT, "evidence.json"),
    JSON.stringify(evidence, null, 2),
  );
  console.log(
    `EVIDENCE ${path.join(ROOT, "evidence.json")} result=${evidence.result} cleanup=${report.clean ? "clean" : "DIRTY"}`,
  );
  const code =
    evidence.result === "passed" && report.clean
      ? 0
      : evidence.result === "timeout"
        ? 2
        : 1;
  process.exit(code);
}
for (const event of ["uncaughtException", "unhandledRejection"]) {
  process.on(event, (error) => void finish(event, error));
}
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => void finish(`interrupted (${signal})`));
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(what, check, timeoutMs = 40_000, everyMs = 200) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    try {
      last = await check();
      if (last) return last;
    } catch (error) {
      last = error;
    }
    await sleep(everyMs);
  }
  throw new Error(
    `timed out waiting for ${what} (last: ${last?.message ?? JSON.stringify(last)})`,
  );
}

async function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
    server.on("error", reject);
  });
}

// ----------------------------------------------------------------- central --

const PORT = await freePort();
const ORIGIN = `http://localhost:${PORT}`;
const CENTRAL_DB = path.join(ROOT, "central/central.sqlite");
const centralEnv = {
  PATH: "/usr/bin:/bin",
  HOME: path.join(ROOT, "central"),
  TMPDIR: path.join(ROOT, "central"),
  BIND_ADDR: `127.0.0.1:${PORT}`,
  DATABASE_URL: `sqlite://${CENTRAL_DB}?mode=rwc`,
  STATIC_DIR: DIST,
  AUTH_ENABLED: "true",
  PASSWORD_AUTH_ENABLED: "true",
  COOKIE_SECURE: "false",
  SHUTDOWN_DRAIN_SECS: "0",
  RUST_LOG: "info",
};

async function startCentral() {
  const log = fs.openSync(path.join(ROOT, "logs/central.log"), "a");
  evidence.commands.push({ central: SERVER_BIN, env: centralEnv });
  const child = own(
    spawn(SERVER_BIN, [], { env: centralEnv, stdio: ["ignore", log, log] }),
    "central",
  );
  await child.started;
  return child;
}

// --------------------------------------------------------- account helpers --

/** A cookie jar per account, talking to central the way a browser would. */
class Account {
  constructor(email) {
    this.email = email;
    this.cookie = "";
  }
  async request(method, url, body) {
    const response = await fetch(`${ORIGIN}${url}`, {
      method,
      headers: {
        ...(body ? { "Content-Type": "application/json" } : {}),
        ...(this.cookie ? { Cookie: this.cookie } : {}),
      },
      body: body ? JSON.stringify(body) : undefined,
      redirect: "manual",
    });
    const set = response.headers.getSetCookie?.() ?? [];
    if (set.length) this.cookie = set.map((c) => c.split(";")[0]).join("; ");
    return response;
  }
  async register() {
    const response = await this.request("POST", "/auth/register", {
      email: this.email,
      password: PASSWORD,
    });
    if (!response.ok)
      throw new Error(
        `register ${this.email}: ${response.status} ${await response.text()}`,
      );
    return this;
  }
}

// -------------------------------------------------------------------- host --

const HOST_HOME = path.join(ROOT, "host");
// The runtime dir holds Unix sockets, whose paths must stay short (SUN_LEN):
// a fresh, owned directory under a short per-user parent.
const RUNTIME = fs.mkdtempSync(
  path.join(env("PRIVATE_APP_RUNTIME_PARENT"), "pa-"),
);
evidence.runtime = RUNTIME;
const SETTINGS = path.join(HOST_HOME, "local-host.json");
fs.writeFileSync(
  SETTINGS,
  JSON.stringify({
    transport: { candidate_ips: ["127.0.0.1"] },
    webauthn: { rp_id: "localhost", origins: [ORIGIN] },
    publication: { account: OWNER },
  }),
);
const hostEnv = {
  PATH: `${STUBS}:/usr/bin:/bin`,
  HOME: HOST_HOME,
  TMPDIR: path.join(HOST_HOME, "tmp"),
  XDG_DATA_HOME: path.join(HOST_HOME, "data"),
  XDG_CONFIG_HOME: path.join(HOST_HOME, "config"),
  CONVERSATIONS_HOST_SETTINGS_FILE: SETTINGS,
  CONVERSATIONS_SERVER: ORIGIN,
};

// The recording agent: writes a structured Claude transcript, records what it
// is sent, appends history on request, and exits on the fixture's stop file.
fs.writeFileSync(
  path.join(STUBS, "claude"),
  `#!/usr/bin/python3
import json, os, select, subprocess, sys, time
from pathlib import Path
stubs = Path(${JSON.stringify(STUBS)})
args = sys.argv[1:]
session = next((args[i + 1] for i, a in enumerate(args[:-1]) if a in ('--session-id', '--resume')), '')
with (stubs / 'claude.runs').open('a') as runs:
    runs.write(' '.join(args) + '\\n')
project = os.getcwd().replace('.', '-').replace('/', '-')
directory = Path(os.environ['HOME']) / '.claude' / 'projects' / project
directory.mkdir(parents=True, exist_ok=True)
path = directory / (session + '.jsonl')
def line(uuid, role, content, ts):
    message = {'role': role, 'content': content}
    if role == 'assistant':
        message.update({'id': 'msg_' + uuid, 'model': 'stub'})
    return json.dumps({'type': role, 'uuid': session + '-' + uuid, 'sessionId': session,
        'timestamp': ts, 'message': message}) + '\\n'
with path.open('a') as t:
    t.write(line('u1', 'user', 'private transcript ${TRANSCRIPT_MARK} of ' + session, '2026-11-03T10:00:00.000Z'))
    t.write(line('a1', 'assistant', [{'type': 'text', 'text': 'checking'},
        {'type': 'tool_use', 'id': 'toolu_1', 'name': 'Bash', 'input': {'command': 'grep -r ${TRANSCRIPT_MARK} .'}}],
        '2026-11-03T10:00:01.000Z'))
subprocess.run(['/bin/stty', '-echo', '-icanon'], stderr=subprocess.DEVNULL)
os.write(1, b'screen ${SCREEN_MARK}\\r\\n')
done = set()
seen_drop = False
deadline = time.monotonic() + 600
try:
    with (stubs / ('claude.stdin.' + session)).open('ab', buffering=0) as received:
        while stubs.exists() and not (stubs / 'stop').exists() and time.monotonic() < deadline:
            for name, mark, n in (('mutate', '${MUTATION_MARK}', 2), ('after-unshare', '${AFTER_UNSHARE_MARK}', 3)):
                if name not in done and (stubs / (name + '.' + session)).exists():
                    done.add(name)
                    with path.open('a') as t:
                        t.write(line('u%d' % n, 'user', 'later ' + mark, '2026-11-03T10:00:0%d.000Z' % n))
            say = stubs / ('say.' + session)
            if say.exists():
                os.write(1, b'screen ' + say.read_bytes() + b'\\r\\n')
                say.unlink()
            ready, _, _ = select.select([0], [], [], 0.1)
            if ready:
                data = os.read(0, 4096)
                if not data:
                    break
                received.write(data)
                if not seen_drop and b'${FILE_NAME}' in Path(stubs / ('claude.stdin.' + session)).read_bytes():
                    seen_drop = True
                    os.write(1, b'saw the drop ${UPDATE_MARK}\\r\\n')
finally:
    if stubs.exists():
        (stubs / ('claude.done.' + session)).write_text('exited')
`,
  { mode: 0o700 },
);

function hostCli(args) {
  const full = ["--server", ORIGIN, ...args];
  evidence.commands.push({ host: [HOST_BIN, ...full].join(" ") });
  const out = spawnSync(HOST_BIN, full, {
    env: hostEnv,
    cwd: path.join(HOST_HOME, "work"),
    encoding: "utf8",
    timeout: 30_000,
  });
  if (out.status !== 0)
    throw new Error(`${args.join(" ")} failed: ${out.stderr || out.stdout}`);
  return out.stdout;
}
const hostAdmin = (...args) =>
  hostCli(["host", "--runtime-dir", RUNTIME, ...args]);

let hostChild = null;
async function startHost() {
  const log = fs.openSync(path.join(ROOT, "logs/host.log"), "a");
  const args = [
    "--server",
    ORIGIN,
    "host",
    "--runtime-dir",
    RUNTIME,
    "run",
    "--backend",
    "normal",
  ];
  evidence.commands.push({ hostRun: [HOST_BIN, ...args].join(" ") });
  hostChild = own(
    spawn(HOST_BIN, args, {
      env: hostEnv,
      cwd: path.join(HOST_HOME, "work"),
      stdio: ["ignore", log, log],
    }),
    "host",
  );
  await hostChild.started;
}
async function stopHost() {
  const child = hostChild;
  hostChild = null;
  if (!child || child.exitCode !== null) return;
  child.kill("SIGTERM");
  await Promise.race([child.exited, sleep(10_000)]);
  if (child.exitCode === null && child.signalCode === null) {
    child.kill("SIGKILL");
    await Promise.race([child.exited, sleep(5_000)]);
  }
}

// --------------------------------------------------------------------- CDP --

class Cdp {
  static async launch() {
    const profile = path.join(ROOT, "chrome");
    const child = own(
      spawn(
        CHROME,
        [
          "--headless=new",
          "--remote-debugging-port=0",
          "--remote-debugging-address=127.0.0.1",
          "--no-first-run",
          "--no-default-browser-check",
          "--disable-background-networking",
          "--disable-component-update",
          "--disable-sync",
          "--disable-extensions",
          "--password-store=basic",
          "--use-mock-keychain",
          "--allow-loopback-in-peer-connection",
          "--disable-features=WebRtcHideLocalIpsWithMdns",
          `--user-data-dir=${profile}`,
          "about:blank",
        ],
        {
          stdio: "ignore",
          env: { PATH: "/usr/bin:/bin", HOME: profile, TMPDIR: profile },
        },
      ),
      "chrome",
    );
    await child.started;
    const portFile = path.join(profile, "DevToolsActivePort");
    await waitFor("Chromium DevTools port", () => fs.existsSync(portFile));
    const [port, browserPath] = fs
      .readFileSync(portFile, "utf8")
      .trim()
      .split("\n");
    const cdp = new Cdp(
      new WebSocket(`ws://127.0.0.1:${port}${browserPath}`),
      child,
    );
    await new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error("CDP socket did not open")),
        CDP_TIMEOUT_MS,
      );
      cdp.socket.addEventListener(
        "open",
        () => (clearTimeout(timer), resolve()),
        { once: true },
      );
      cdp.socket.addEventListener(
        "error",
        () => (clearTimeout(timer), reject(new Error("CDP socket failed"))),
        { once: true },
      );
    });
    return cdp;
  }
  constructor(socket, child) {
    this.socket = socket;
    this.child = child;
    this.next = 1;
    this.pending = new Map();
    this.listeners = [];
    socket.addEventListener("close", () => {
      for (const { reject, method } of this.pending.values())
        reject(new Error(`${method}: CDP connection closed`));
      this.pending.clear();
    });
    socket.addEventListener("message", (event) => {
      const message = JSON.parse(event.data);
      if (message.id && this.pending.has(message.id)) {
        const { resolve, reject, method } = this.pending.get(message.id);
        this.pending.delete(message.id);
        if (message.error)
          reject(new Error(`${method}: ${JSON.stringify(message.error)}`));
        else resolve(message.result);
      } else if (message.method) {
        for (const listener of this.listeners) listener(message);
      }
    });
  }
  send(method, params = {}, sessionId) {
    const id = this.next++;
    if (!(FAULT === "hang-cdp" && method === "Target.createTarget")) {
      this.socket.send(
        JSON.stringify({
          id,
          method,
          params,
          ...(sessionId ? { sessionId } : {}),
        }),
      );
    }
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${method} timed out`));
      }, CDP_TIMEOUT_MS);
      this.pending.set(id, {
        method,
        resolve: (v) => (clearTimeout(timer), resolve(v)),
        reject: (e) => (clearTimeout(timer), reject(e)),
      });
    });
  }
  async page(name, { context } = {}) {
    const { targetId } = await this.send("Target.createTarget", {
      url: "about:blank",
      ...(context ? { browserContextId: context } : {}),
    });
    const { sessionId } = await this.send("Target.attachToTarget", {
      targetId,
      flatten: true,
    });
    return new Page(this, sessionId, name);
  }
}

/** Every page opened, so a failed run can record what each one showed. */
const pages = [];

class Page {
  constructor(cdp, sessionId, name) {
    this.cdp = cdp;
    this.sessionId = sessionId;
    this.name = name;
    this.requests = [];
    this.console = [];
    pages.push(this);
    cdp.listeners.push((message) => {
      if (message.sessionId !== sessionId) return;
      if (message.method === "Runtime.consoleAPICalled") {
        const { type, args } = message.params;
        this.console.push(
          `${type}: ${args.map((a) => a.value ?? a.description ?? "").join(" ")}`.slice(
            0,
            500,
          ),
        );
      }
      if (message.method === "Runtime.exceptionThrown")
        this.console.push(
          `exception: ${message.params.exceptionDetails?.exception?.description ?? message.params.exceptionDetails?.text}`.slice(
            0,
            500,
          ),
        );
      if (message.method === "Network.requestWillBeSent") {
        const r = message.params.request;
        this.requests.push({
          id: message.params.requestId,
          method: r.method,
          url: r.url,
          body: r.postData ?? null,
          hasBody: !!r.hasPostData,
        });
      }
    });
  }
  send(method, params) {
    return this.cdp.send(method, params, this.sessionId);
  }
  /**
   * Stop this page's scripts, as a throttled or sleeping tab would: nothing
   * runs (no lease renewal) until `resume`; the network keeps going.
   */
  async pause() {
    await this.send("Debugger.enable");
    const paused = new Promise((resolve) => {
      const listener = (message) => {
        if (
          message.sessionId === this.sessionId &&
          message.method === "Debugger.paused"
        ) {
          this.cdp.listeners.splice(this.cdp.listeners.indexOf(listener), 1);
          resolve();
        }
      };
      this.cdp.listeners.push(listener);
    });
    this.paused = true;
    await this.send("Debugger.pause");
    await Promise.race([
      paused,
      sleep(15_000).then(() => {
        throw new Error(`${this.name} did not pause`);
      }),
    ]);
  }
  async resume() {
    if (!this.paused) return;
    this.paused = false;
    await this.send("Debugger.resume");
    await this.send("Debugger.disable");
  }
  async init({ network = false } = {}) {
    await this.send("Page.enable");
    await this.send("Runtime.enable");
    if (network)
      await this.send("Network.enable", { maxPostDataSize: 1 << 20 });
  }
  async goto(url) {
    await this.send("Page.navigate", {
      url: url.startsWith("http") ? url : ORIGIN + url,
    });
    await waitFor(`${this.name} load ${url}`, () =>
      this.eval("document.readyState === 'complete'"),
    );
  }
  async eval(expression) {
    const reply = await this.send("Runtime.evaluate", {
      expression,
      awaitPromise: true,
      returnByValue: true,
    });
    if (reply.exceptionDetails)
      throw new Error(
        `${this.name}: ${JSON.stringify(reply.exceptionDetails).slice(0, 500)}`,
      );
    return reply.result.value;
  }
  text() {
    return this.eval("document.body ? document.body.innerText : ''");
  }
  async waitText(needle, timeoutMs = 40_000) {
    return waitFor(
      `${this.name} text ${JSON.stringify(needle)}`,
      async () => (await this.text()).includes(needle),
      timeoutMs,
    );
  }
  /** Click the first visible, enabled button (or link) whose text or aria-label is exactly `label`. */
  async click(label, timeoutMs = 40_000) {
    await waitFor(
      `${this.name} button ${JSON.stringify(label)}`,
      () =>
        this.eval(`(() => {
        const want = ${JSON.stringify(label)};
        const el = [...document.querySelectorAll('button, a, [role=radio], [role=menuitem]')].find((b) =>
          (b.innerText.trim() === want || b.getAttribute('aria-label') === want) && !b.disabled && b.offsetParent !== null);
        if (!el) return false;
        el.click();
        return true;
      })()`),
      timeoutMs,
    );
  }
  async select(label, value) {
    await waitFor(`${this.name} select ${label}`, () =>
      this.eval(`(() => {
      const select = document.querySelector('select[aria-label="' + ${JSON.stringify(label)} + '"]');
      if (!select || ![...select.options].some((option) => option.value === ${JSON.stringify(value)})) return false;
      select.value = ${JSON.stringify(value)};
      select.dispatchEvent(new Event('change', { bubbles: true }));
      return true;
    })()`),
    );
  }
  async showHistory() {
    await waitFor("open the running session transcript", () =>
      this.eval(`(() => {
        const button = [...document.querySelectorAll('button')].find((b) => b.innerText.includes('Show full conversation'));
        if (!button) return false;
        button.click();
        return true;
      })()`),
    );
  }
  /** Focus the input labelled `label` and type `text` as a user would. */
  async type(label, text) {
    await waitFor(`${this.name} input ${JSON.stringify(label)}`, () =>
      this.eval(`(() => {
        const want = ${JSON.stringify(label)};
        const byAria = document.querySelector('[aria-label="' + want + '"]');
        const byLabel = [...document.querySelectorAll('label')].find((l) => l.innerText.trim().startsWith(want))?.querySelector('input, textarea');
        const el = byAria || byLabel;
        if (!el) return false;
        el.focus();
        return document.activeElement === el;
      })()`),
    );
    await this.send("Input.insertText", { text });
  }
  /** A real (trusted) mouse click at the centre of the first match. */
  async mouseClick(selector) {
    const at = await waitFor(`${this.name} element ${selector}`, () =>
      this.eval(`(() => {
        const el = document.querySelector(${JSON.stringify(selector)});
        if (!el || el.offsetParent === null) return null;
        el.scrollIntoView({ block: "center" });
        const r = el.getBoundingClientRect();
        return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
      })()`),
    );
    for (const type of ["mousePressed", "mouseReleased"])
      await this.send("Input.dispatchMouseEvent", {
        type,
        x: at.x,
        y: at.y,
        button: "left",
        clickCount: 1,
      });
  }
  async press(key) {
    const code = { Enter: 13 }[key];
    await this.send("Input.dispatchKeyEvent", {
      type: "keyDown",
      key,
      code: key,
      windowsVirtualKeyCode: code,
      nativeVirtualKeyCode: code,
    });
    await this.send("Input.dispatchKeyEvent", {
      type: "keyUp",
      key,
      code: key,
      windowsVirtualKeyCode: code,
      nativeVirtualKeyCode: code,
    });
  }
  async fetchJson(method, url, body) {
    return this
      .eval(`fetch(${JSON.stringify(url)}, { method: ${JSON.stringify(method)}, credentials: 'include',
      headers: { 'Content-Type': 'application/json' }, body: ${body ? JSON.stringify(JSON.stringify(body)) : "undefined"} })
      .then(async (r) => ({ status: r.status, body: await r.text() }))`);
  }
  async requestBodies() {
    for (const r of this.requests) {
      if (r.hasBody && r.body === null) {
        try {
          r.body = (
            await this.send("Network.getRequestPostData", { requestId: r.id })
          ).postData;
        } catch {
          r.body = "<unavailable>";
        }
      }
    }
    return this.requests;
  }
}

// ------------------------------------------------------------ leak checks --

function fileBytes(file) {
  return fs.existsSync(file) ? fs.readFileSync(file) : Buffer.alloc(0);
}
/** Every byte central persisted: the database and its WAL/journal. */
function centralBytes() {
  return Buffer.concat(
    ["", "-wal", "-journal", "-shm"].map((s) => fileBytes(CENTRAL_DB + s)),
  );
}
function centralHas(mark) {
  return centralBytes().includes(Buffer.from(mark));
}
/** Live rows (a logical dump), as opposed to bytes left in free pages or the WAL. */
function centralRowsHave(mark) {
  const out = spawnSync(
    SQLITE3,
    ["-readonly", "-cmd", ".timeout 5000", CENTRAL_DB, ".dump"],
    {
      encoding: "utf8",
      timeout: 30_000,
      maxBuffer: 256 << 20,
    },
  );
  if (out.status !== 0) throw new Error(`sqlite3 .dump: ${out.stderr}`);
  return out.stdout.includes(mark);
}
function sql(db, query) {
  const out = spawnSync(
    SQLITE3,
    ["-readonly", "-cmd", ".timeout 5000", db, query],
    { encoding: "utf8", timeout: 15_000 },
  );
  if (out.status !== 0) throw new Error(`sqlite3: ${out.stderr}`);
  return out.stdout.trim();
}

// ------------------------------------------------------------------ journey --

const watchdog = setTimeout(() => {
  evidence.result = "timeout";
  void finish("timeout", new Error(`overall timeout ${OVERALL_TIMEOUT_MS}ms`));
}, OVERALL_TIMEOUT_MS);

let failure = null;
let central;
try {
  central = await startCentral();
  await waitFor(
    "central /healthz",
    async () => (await fetch(`${ORIGIN}/healthz`)).ok,
  );
  step("central up", { origin: ORIGIN, db: CENTRAL_DB, dist: DIST });

  // The host publishes as its owner's account; it logs in like the CLI does.
  hostCli(["login", "--register", "--email", OWNER, "--password", PASSWORD]);
  await startHost();
  let turnPort = null;
  if (TURN_BIN) {
    turnPort = await freePort();
    const turnArgs = [
      "-n",
      "--no-cli",
      "--no-tls",
      "--no-dtls",
      "--listening-ip=127.0.0.1",
      "--relay-ip=127.0.0.1",
      `--listening-port=${turnPort}`,
      "--lt-cred-mech",
      `--user=${TURN_USER}:${TURN_GOOD}`,
      "--realm=acme.test",
      "--allow-loopback-peers",
      "--verbose",
      `--log-file=${path.join(ROOT, "logs/turn.log")}`,
      "--simple-log",
      `--pidfile=${path.join(ROOT, "turn.pid")}`,
    ];
    evidence.commands.push({ turn: [TURN_BIN, ...turnArgs].join(" ") });
    const turnOut = fs.openSync(path.join(ROOT, "logs/turn.stdout.log"), "a");
    const turn = own(
      spawn(TURN_BIN, turnArgs, {
        env: { PATH: "/usr/bin:/bin", HOME: path.join(ROOT, "turn-home") },
        stdio: ["ignore", turnOut, turnOut],
      }),
      "coturn",
    );
    await turn.started;
    step("coturn up", { port: turnPort });
  }
  const turnLog = () => {
    const f = path.join(ROOT, "logs/turn.log");
    return fs.existsSync(f) ? fs.readFileSync(f, "utf8") : "";
  };
  const hostId = hostAdmin("credential", "identity").trim().split(/\s+/).pop();
  const descriptor = hostAdmin("credential", "descriptor")
    .trim()
    .split(/\s+/)
    .find((w) => w.startsWith("chd1."));
  if (!descriptor) throw new Error("no host descriptor printed");
  const ownerApi = await new Account(OWNER)
    .request("POST", "/auth/login", { email: OWNER, password: PASSWORD })
    .then(async (r) => {
      if (!r.ok) throw new Error(`owner login ${r.status}`);
      return r;
    });
  void ownerApi;
  step("host up", { hostId });
  if (FAULT === "after-startup")
    throw new Error("injected fault after startup");

  const cdp = await Cdp.launch();
  const owner = await cdp.page("owner");
  await owner.init({ network: true });
  if (TURN_BIN) {
    // Read-only: remember each RTCPeerConnection the app creates, unchanged.
    await owner.send("Page.addScriptToEvaluateOnNewDocument", {
      source: `(() => {
        const Native = window.RTCPeerConnection;
        window.__observedPcs = [];
        window.RTCPeerConnection = new Proxy(Native, {
          construct(target, args, newTarget) {
            const pc = Reflect.construct(target, args, newTarget);
            window.__observedPcs.push(pc);
            return pc;
          },
        });
      })();`,
    });
  }
  await owner.send("WebAuthn.enable", { enableUI: false });
  const { authenticatorId } = await owner.send(
    "WebAuthn.addVirtualAuthenticator",
    {
      options: {
        protocol: "ctap2",
        transport: "internal",
        hasResidentKey: true,
        hasUserVerification: true,
        isUserVerified: true,
        automaticPresenceSimulation: true,
      },
    },
  );
  await owner.goto("/login");
  const login = await owner.fetchJson("POST", "/auth/login", {
    email: OWNER,
    password: PASSWORD,
  });
  if (login.status >= 400)
    throw new Error(`browser owner login: ${JSON.stringify(login)}`);
  await waitFor("relay shows host online", async () => {
    const r = await owner.fetchJson("GET", `/api/host-relay/${hostId}/status`);
    return r.status === 200 && JSON.parse(r.body).online === true;
  });

  // ---- Settings: pair from the descriptor the host printed ----
  await owner.goto("/settings");
  await owner.type("Host descriptor", descriptor);
  await owner.type("Name", "laptop");
  await owner.click("Pair host");
  await owner.waitText(hostId);
  step("paired in Settings", { hostId });

  // ---- passkey registration from the host's link, approved LOCALLY ----
  const grants = [
    "session.start",
    "session.reopen",
    "session.write",
    "session.manage_sharing",
    "history.read",
    "host.settings_read",
    "host.settings_write",
    "passkey.manage",
  ];
  const started = hostAdmin(
    "passkey",
    "enroll-start",
    "--label",
    "phone",
    ...grants.flatMap((g) => ["--grant", g]),
  );
  const link = started
    .split(/\s+/)
    .find((w) => w.startsWith(`${ORIGIN}/host-link#`));
  if (!link) throw new Error(`no enrollment link in: ${started.slice(0, 400)}`);
  await owner.goto(link);
  await owner.waitText("Connect this device");
  const registerLabel = await waitFor("register button", () =>
    owner.eval(
      `[...document.querySelectorAll('button')].map((b) => b.innerText.trim()).find((t) => /passkey/i.test(t)) || null`,
    ),
  );
  await owner.click(registerLabel);
  await waitFor(
    "registration done",
    async () =>
      /approve|registered|created/i.test(await owner.text()) &&
      !/not valid/i.test(await owner.text()),
  );
  const pending = hostAdmin("passkey", "pending");
  const pendingId = JSON.parse(pending).pending?.find(
    (p) => p.label === "phone",
  )?.cred_id;
  if (!pendingId) throw new Error(`no pending registration in: ${pending}`);
  // Ids are base64url and may start with "-": end the options first.
  hostAdmin("passkey", "approve", "--", pendingId);
  const credentials = await owner.send("WebAuthn.getCredentials", {
    authenticatorId,
  });
  step("passkey registered and approved locally", {
    pendingId,
    authenticatorCredentials: credentials.credentials.length,
  });

  // ---- unlock: passkey assertion -> 24h browser key, no local approval ----
  await owner.goto("/settings");
  await owner.click("Unlock with passkey");
  await owner.waitText("Unlocked until");
  step("unlocked with passkey");

  // ---- start a session from the browser (signed, private channel) ----
  await owner.goto("/");
  await owner.click("Start new session");
  const runs = path.join(STUBS, "claude.runs");
  const session = await waitFor(
    "stub agent started",
    () => {
      if (!fs.existsSync(runs)) return null;
      const m = fs
        .readFileSync(runs, "utf8")
        .match(/--session-id ([0-9a-f-]{36})/);
      return m?.[1] ?? null;
    },
    60_000,
  );
  const history = path.join(RUNTIME, "history.db");
  await waitFor(
    "local history holds the transcript",
    () =>
      fs.existsSync(history) &&
      sql(
        history,
        `SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH '${TRANSCRIPT_MARK}'`,
      ) !== "0",
    60_000,
  );
  step("session started from the browser", { session });

  if (TURN_BIN) {
    // Replace the value of the input labelled `label` as a user would.
    const fill = async (label, text) => {
      await waitFor(`input ${label}`, () =>
        owner.eval(`(() => {
          const scope = document.querySelector('[data-testid="ice-server-0"]') ?? document;
          const l = [...scope.querySelectorAll('label')].find((x) => x.innerText.trim().startsWith(${JSON.stringify(label)}));
          const el = l?.querySelector('input, textarea');
          if (!el) return false;
          el.focus();
          el.select();
          return document.activeElement === el;
        })()`),
      );
      await owner.send("Input.insertText", { text });
    };
    const pcSummary = () =>
      owner.eval(`(async () => {
        const out = [];
        for (const pc of window.__observedPcs ?? []) {
          const c = pc.getConfiguration();
          const stats = await pc.getStats();
          let selected = null;
          const byId = new Map();
          stats.forEach((r) => byId.set(r.id, r));
          stats.forEach((r) => {
            if (r.type === 'transport' && r.selectedCandidatePairId) {
              const pair = byId.get(r.selectedCandidatePairId);
              const local = pair && byId.get(pair.localCandidateId);
              const remote = pair && byId.get(pair.remoteCandidateId);
              selected = { state: pair?.state, local: local?.candidateType, localProtocol: local?.protocol, relayProtocol: local?.relayProtocol, remote: remote?.candidateType };
            }
          });
          const localTypes = [];
          stats.forEach((r) => { if (r.type === 'local-candidate') localTypes.push(r.candidateType); });
          out.push({ policy: c.iceTransportPolicy, servers: (c.iceServers ?? []).map((s) => s.urls), connection: pc.connectionState, ice: pc.iceConnectionState, selected, localTypes });
        }
        return out;
      })()`);
    // In-app navigation keeps this page load, and with it any cached channel.
    const navigate = async (to) => {
      const how = await owner.eval(
        `(() => { const a = [...document.querySelectorAll('a[href="${to}"]')].find((x) => x.offsetParent !== null); if (a) { a.click(); return "link"; } history.pushState({}, "", "${to}"); dispatchEvent(new PopStateEvent("popstate")); return "history"; })()`,
      );
      evidence.turn.navigations = [
        ...(evidence.turn.navigations ?? []),
        `${to}:${how}`,
      ];
      await waitFor(`at ${to}`, () =>
        owner.eval(`location.pathname === ${JSON.stringify(to)}`),
      );
    };
    evidence.turn = { port: turnPort };
    // Direct first, in THIS page load: its channel is then cached.
    await owner.goto("/settings");
    await navigate("/private");
    await waitFor(
      "direct: the private list shows the session",
      () =>
        owner.eval(
          `!!document.querySelector('a[href^="/private/${session}"]')`,
        ),
      60_000,
    );
    const directPcs = await pcSummary();
    evidence.turn.direct = directPcs;
    if (!directPcs.some((p) => p.selected && p.selected.local !== "relay"))
      throw new Error(`no direct channel first: ${JSON.stringify(directPcs)}`);
    step("direct private channel established first", { pcs: directPcs });

    // ---- relay-only with a WRONG credential, saved in the visible UI ----
    await navigate("/settings");
    // The form may already offer an empty server row; add one only if not.
    await waitFor("network settings form", () =>
      owner.eval(`!!document.querySelector('#host-network-heading')`),
    );
    if (
      !(await owner.eval(
        `!!document.querySelector('[data-testid="ice-server-0"]')`,
      ))
    )
      await owner.click("Add server");
    await fill(
      "URLs (one per line)",
      `turn:127.0.0.1:${turnPort}?transport=udp`,
    );
    await fill("Username", TURN_USER);
    await fill("Credential", TURN_BAD);
    await owner.eval(
      `(() => { const box = [...document.querySelectorAll('label')].find((l) => l.innerText.includes('Relay only'))?.querySelector('input[type=checkbox]'); if (box && !box.checked) box.click(); return box?.checked ?? false; })()`,
    );
    await owner.click("Save network settings");
    await sleep(500);
    const saveProblem = await owner.eval(
      `document.querySelector('form [role=alert]')?.innerText ?? null`,
    );
    if (saveProblem) throw new Error(`settings not saved: ${saveProblem}`);
    // Read-only: the UI shows the saved mode, and this browser stored exactly
    // these settings for this host.
    const turnUrl = `turn:127.0.0.1:${turnPort}?transport=udp`;
    const savedMode = await owner.eval(
      `document.querySelector('[data-testid="network-mode"]')?.innerText ?? null`,
    );
    const stored = await owner.eval(`(() => {
      const all = JSON.parse(localStorage.getItem("conversations.hostNetwork") ?? "null");
      const entry = all?.[${JSON.stringify(hostId)}];
      const server = entry?.network?.iceServers?.[0];
      return {
        hosts: all ? Object.keys(all).length : 0,
        rev: entry?.rev ?? null,
        policy: entry?.network?.iceTransportPolicy ?? null,
        servers: entry?.network?.iceServers?.length ?? 0,
        urls: server?.urls ?? null,
        username: server?.username === ${JSON.stringify(TURN_USER)},
        badCredential: server?.credential === ${JSON.stringify(TURN_BAD)},
      };
    })()`);
    evidence.turn.badSaved = { mode: savedMode, stored };
    if (!/^Relay only/.test(savedMode ?? ""))
      throw new Error(`network mode after save: ${savedMode}`);
    if (!(
      stored.policy === "relay" &&
      stored.servers === 1 &&
      (stored.urls === turnUrl ||
        (Array.isArray(stored.urls) &&
          stored.urls.length === 1 &&
          stored.urls[0] === turnUrl)) &&
      stored.username &&
      stored.badCredential &&
      Number.isSafeInteger(stored.rev) &&
      stored.rev >= 1
    ))
      throw new Error(`stored host network: ${JSON.stringify(stored)}`);
    const stdinBefore = (() => {
      const f = path.join(STUBS, `claude.stdin.${session}`);
      return fs.existsSync(f) ? fs.readFileSync(f, "utf8") : "";
    })();
    const sessionsBefore = fs.readFileSync(
      path.join(STUBS, "claude.runs"),
      "utf8",
    );
    // Same page load: the cached direct channel must not be reused. Try a
    // real private effect (a reply to the agent) under the wrong credential.
    await navigate("/private");
    await waitFor(
      "the (cached) list link",
      () =>
        owner.eval(
          `!!document.querySelector('a[href^="/private/${session}"]')`,
        ),
      60_000,
    ).catch(() => null);
    const sessionLink = await owner.eval(
      `(() => { const a = document.querySelector('a[href^="/private/${session}"]'); if (!a) return false; a.click(); return true; })()`,
    );
    let replyTried = false;
    if (sessionLink) {
      const replyBox = await waitFor(
        "reply box",
        () => owner.eval(`!!document.querySelector('[aria-label="Reply"]')`),
        60_000,
      ).catch(() => false);
      if (replyBox) {
        await owner.type("Reply", BAD_REPLY_MARK);
        await owner.click("Send");
        replyTried = true;
      }
    }
    const badShown = await waitFor(
      "a wrong TURN credential fails visibly",
      async () => {
        const t = await owner.text();
        const m = t.match(
          /[^\n]*(not reachable privately|did not answer|could not|failed|unreachable)[^\n]*/i,
        );
        return m?.[0] ?? null;
      },
      120_000,
    ).catch(async (error) => {
      evidence.turn.badTimeout = {
        sessionLink,
        replyTried,
        badReplyInStdin: (() => {
          const f = path.join(STUBS, `claude.stdin.${session}`);
          return (
            fs.existsSync(f) &&
            fs.readFileSync(f, "utf8").includes(BAD_REPLY_MARK)
          );
        })(),
        url: await owner.eval("location.pathname"),
        pcs: await pcSummary(),
        allocations: (turnLog().match(/ALLOCATE processed, success/g) ?? [])
          .length,
        text: (await owner.text()).slice(-1500),
      };
      throw error;
    });
    await sleep(5_000);
    const badPcs = await pcSummary();
    const stdinAfter = (() => {
      const f = path.join(STUBS, `claude.stdin.${session}`);
      return fs.existsSync(f) ? fs.readFileSync(f, "utf8") : "";
    })();
    const badEvidence = {
      shown: badShown,
      sessionLink,
      replyTried,
      pcs: badPcs,
      agentGotBadReply: stdinAfter.includes(BAD_REPLY_MARK),
      agentInputChanged: stdinAfter !== stdinBefore,
      agentRunsChanged:
        fs.readFileSync(path.join(STUBS, "claude.runs"), "utf8") !==
        sessionsBefore,
      turnLog401: (turnLog().match(/401/g) ?? []).length,
      turnAllocationsSoFar: (
        turnLog().match(/ALLOCATE processed, success/g) ?? []
      ).length,
    };
    evidence.turn.bad = badEvidence;
    const newPcs = badPcs.slice(directPcs.length);
    if (
      badEvidence.agentGotBadReply ||
      badEvidence.agentInputChanged ||
      badEvidence.agentRunsChanged ||
      newPcs.some((p) => p.connection === "connected") ||
      newPcs.some((p) => p.policy !== "relay") ||
      badEvidence.turnAllocationsSoFar > 0
    )
      throw new Error(
        `wrong TURN credential still reached the host: ${JSON.stringify(badEvidence)}`,
      );
    step(
      "relay-only with a wrong credential: no channel, no fallback, no effect",
      badEvidence,
    );

    // ---- corrected in the UI, persisted across a reload ----
    await owner.goto("/settings");
    await fill("Credential", TURN_GOOD);
    await owner.click("Save network settings");
    await sleep(500);
    await owner.goto("/settings");
    const readPersisted = () =>
      owner.eval(`(() => {
      const urls = [...document.querySelectorAll('label')].find((l) => l.innerText.trim().startsWith('URLs'))?.querySelector('textarea')?.value;
      const relay = [...document.querySelectorAll('label')].find((l) => l.innerText.includes('Relay only'))?.querySelector('input')?.checked;
      return { urls, relay };
    })()`);
    // The form renders after the reload; wait for both fields to exist.
    await waitFor(
      "the network form after the reload",
      async () => {
        const r = await readPersisted();
        return r.urls !== undefined && r.relay !== undefined;
      },
      30_000,
    ).catch(() => null);
    const persisted = await readPersisted();
    if (!persisted.urls?.includes(String(turnPort)) || !persisted.relay)
      throw new Error(`settings did not persist: ${JSON.stringify(persisted)}`);
    step("corrected credential saved and persisted across a reload", persisted);
  }

  // ---- /private: list, structured history, search, reply ----
  await owner.goto("/private");
  await waitFor(
    "private list shows the session",
    () =>
      owner.eval(`!!document.querySelector('a[href^="/private/${session}"]')`),
    60_000,
  );
  // Running cards deep-link fullscreen; this journey also exercises history.
  await owner.goto(`/private/${session}`);
  await owner.showHistory();
  await owner.waitText(TRANSCRIPT_MARK, 60_000);
  await owner.waitText("Bash");
  step("private history rendered", { structured: true });
  await owner.type("Search this session", TRANSCRIPT_MARK);
  await owner.click("Search");
  await waitFor("search results", () =>
    owner.eval(
      `(() => { const l = document.querySelector('[aria-label="Search results"]'); return !!l && l.querySelectorAll('a').length > 0; })()`,
    ),
  );
  await owner.type("Reply", REPLY_MARK);
  await owner.click("Send");
  await waitFor("the agent received the reply", () => {
    const f = path.join(STUBS, `claude.stdin.${session}`);
    return fs.existsSync(f) && fs.readFileSync(f, "utf8").includes(REPLY_MARK);
  });
  step("search and reply over the private channel");

  // ---- the terminal, a file drop and an image paste, only on the channel ----
  const stdinOf = () => {
    const f = path.join(STUBS, `claude.stdin.${session}`);
    return fs.existsSync(f) ? fs.readFileSync(f, "utf8") : "";
  };
  const screenText = () =>
    owner.eval(
      `document.querySelector('[data-testid="live-terminal-grid"] .xterm-rows')?.textContent ?? ''`,
    );
  const displayMenu = async () =>
    owner.mouseClick(
      await owner.eval(
        `document.querySelector('button[aria-label="Display"]') ? 'button[aria-label="Display"]' : 'button[aria-label="Actions"]'`,
      ),
    );
  // Fullscreen draws with xterm's DOM renderer, so the screen can be read.
  await displayMenu();
  await owner.click("Fullscreen");
  await waitFor(
    "the private terminal shows the agent's screen",
    async () => (await screenText()).includes(SCREEN_MARK),
    60_000,
  );
  step("private terminal snapshot rendered");

  const fileBody = `dropped ${FILE_BODY_MARK}\n`;
  await waitFor("the terminal accepts a drop", () =>
    owner.eval(`(() => {
      const el = document.querySelector('[data-testid="live-terminal-screen"]');
      if (!el) return false;
      const dt = new DataTransfer();
      dt.items.add(new File([${JSON.stringify(fileBody)}], ${JSON.stringify(FILE_NAME)}, { type: "text/plain" }));
      for (const type of ["dragenter", "dragover", "drop"])
        el.dispatchEvent(new DragEvent(type, { dataTransfer: dt, bubbles: true, cancelable: true }));
      return true;
    })()`),
  );
  const droppedPath = await waitFor(
    "the agent was handed the staged file's path",
    () =>
      stdinOf().match(
        new RegExp(`(/[^\\s'"]*${FILE_NAME.replace(".", "\\.")})`),
      )?.[1] ?? null,
    60_000,
  );
  const droppedBytes = fs.readFileSync(droppedPath, "utf8");
  if (droppedBytes !== fileBody)
    throw new Error(`staged file differs: ${JSON.stringify(droppedBytes)}`);
  if (/\r\s*$/.test(stdinOf()))
    throw new Error("the drop pressed Enter; it must only paste the path");
  await waitFor(
    "the agent's reaction arrived as a live update",
    async () => (await screenText()).includes(UPDATE_MARK),
    30_000,
  );
  step("file dropped over the private channel", { droppedPath });

  // A 1x1 PNG followed by a marker: the host checks the signed size and hash.
  const png = Buffer.concat([
    Buffer.from(
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
      "base64",
    ),
    Buffer.from(IMAGE_MARK),
  ]);
  const before = stdinOf();
  // Clicking the terminal arms keyboard capture; the paste lands in xterm.
  await owner.mouseClick('[data-testid="live-terminal-grid"]');
  await waitFor("keyboard capture focused the terminal", () =>
    owner.eval(
      `document.activeElement?.classList.contains("xterm-helper-textarea") ?? false`,
    ),
  );
  await owner.eval(`(() => {
    const bytes = Uint8Array.from(atob(${JSON.stringify(png.toString("base64"))}), (c) => c.charCodeAt(0));
    const dt = new DataTransfer();
    dt.items.add(new File([bytes], "paste.png", { type: "image/png" }));
    (document.activeElement ?? document).dispatchEvent(
      new ClipboardEvent("paste", { clipboardData: dt, bubbles: true, cancelable: true }),
    );
    return true;
  })()`);
  const pastedPath = await waitFor(
    "the agent was handed the pasted image's path",
    () =>
      stdinOf()
        .slice(before.length)
        .match(/(\/[^\s'"]*\.png)/)?.[1] ?? null,
    60_000,
  );
  if (!fs.readFileSync(pastedPath).equals(png))
    throw new Error("staged image differs from the pasted bytes");
  step("image pasted over the private channel", { pastedPath });
  // Fit the session's grid to this screen: a signed resize, on the channel.
  await displayMenu();
  await owner.click("Fit columns and rows to screen");
  const resized = await waitFor(
    "the host applied the resize",
    async () =>
      (await owner.text()).match(/Terminal is now (\d+×\d+)/)?.[1] ?? null,
    30_000,
  );
  step("terminal resized over the private channel", { resized });
  if (TURN_BIN) {
    const relayPcs = await (async () =>
      owner.eval(`(async () => {
        const out = [];
        for (const pc of window.__observedPcs ?? []) {
          const stats = await pc.getStats();
          const byId = new Map();
          stats.forEach((r) => byId.set(r.id, r));
          let selected = null;
          stats.forEach((r) => {
            if (r.type === 'transport' && r.selectedCandidatePairId) {
              const pair = byId.get(r.selectedCandidatePairId);
              const local = pair && byId.get(pair.localCandidateId);
              const remote = pair && byId.get(pair.remoteCandidateId);
              selected = { state: pair?.state, bytesSent: pair?.bytesSent, bytesReceived: pair?.bytesReceived, local: local?.candidateType, relayProtocol: local?.relayProtocol, localAddress: local?.address + ':' + local?.port, remote: remote?.candidateType };
            }
          });
          out.push({ policy: pc.getConfiguration().iceTransportPolicy, connection: pc.connectionState, selected });
        }
        return out;
      })()`))();
    const log = turnLog();
    const relayEvidence = {
      pcs: relayPcs,
      allocations: (log.match(/ALLOCATE processed, success/g) ?? []).length,
      permissions: (log.match(/CREATE_PERMISSION processed, success/g) ?? [])
        .length,
      channelBinds: (log.match(/CHANNEL_BIND processed, success/g) ?? [])
        .length,
    };
    evidence.turn.relay = relayEvidence;
    const live = relayPcs.filter((p) => p.connection === "connected");
    if (
      !live.length ||
      live.some((p) => p.policy !== "relay" || p.selected?.local !== "relay") ||
      relayEvidence.allocations < 1
    )
      throw new Error(
        `private effects did not go over a TURN relay: ${JSON.stringify(relayEvidence)}`,
      );
    step(
      "effects above went over a relay-selected pair with coturn allocations",
      relayEvidence,
    );
  }

  // ---- a lapsed lease: ONE fresh subscribe, then a visible stop ----
  // The host checks each frame against the subscribe's signed lease and ends
  // the stream as `expired` at the first frame after it. The page is paused
  // (no renewal) past the lease; the agent then prints, so that frame meets
  // the lapsed lease, and only a fresh subscribe's snapshot can show it.
  const sayFile = path.join(STUBS, `say.${session}`);
  const say = async (text) => {
    fs.writeFileSync(`${sayFile}.tmp`, text);
    fs.renameSync(`${sayFile}.tmp`, sayFile);
    await waitFor("the agent printed", () => !fs.existsSync(sayFile));
  };
  const followButton = (label) =>
    owner.eval(
      `[...document.querySelectorAll('button')].some((b) => b.innerText.trim() === ${JSON.stringify(label)} && b.offsetParent !== null)`,
    );
  const lapse = async (mark) => {
    await owner.pause();
    try {
      await sleep(LEASE_LAPSE_MS);
      await say(mark);
      // Let the host meet the lapsed lease with that frame.
      await sleep(3_000);
    } finally {
      await owner.resume();
    }
  };
  await lapse(LEASE_MARK);
  await waitFor(
    "a fresh snapshot shows what the agent printed after the lapse",
    async () => (await screenText()).includes(LEASE_MARK),
    30_000,
  );
  if (await followButton("Follow again"))
    throw new Error("the first lapsed lease stopped the terminal");
  step("lapsed lease: one fresh subscribe repainted the screen");
  await lapse(LEASE_AGAIN_MARK);
  await waitFor(
    "the second lapse stops with a visible Follow again",
    () => followButton("Follow again"),
    30_000,
  );
  // Longer than the reconnect backoff: nothing follows again by itself.
  await sleep(5_000);
  const afterSecondLapse = await screenText();
  if (afterSecondLapse.includes(LEASE_AGAIN_MARK))
    throw new Error("the terminal resubscribed by itself after a second lapse");
  step("second lapse: stopped visibly, no automatic resubscribe");
  await owner.click("Follow again");
  await waitFor(
    "following again (a click) shows the newer screen",
    async () => (await screenText()).includes(LEASE_AGAIN_MARK),
    60_000,
  );
  step("followed again only after a click");
  const transfersOnCentral = {
    screen: centralHas(SCREEN_MARK),
    update: centralHas(UPDATE_MARK),
    fileName: centralHas(FILE_NAME),
    fileBody: centralHas(FILE_BODY_MARK),
    image: centralHas(IMAGE_MARK),
  };
  if (Object.values(transfersOnCentral).some(Boolean))
    throw new Error(
      `terminal or transfer bytes reached central: ${JSON.stringify(transfersOnCentral)}`,
    );
  step("central holds no terminal or transfer bytes", transfersOnCentral);

  // ---- key access: grant then remove, for a key approved LOCALLY ----
  const collaborator = crypto
    .generateKeyPairSync("ed25519")
    .publicKey.export({ format: "der", type: "spki" })
    .subarray(-32);
  const added = hostAdmin(
    "credential",
    "add",
    "--kind",
    "owner",
    "--alg",
    "ed25519",
    "--public-key",
    collaborator.toString("base64"),
    "--label",
    "collaborator",
    "--grant",
    "ceiling:session.write",
    "--grant",
    "ceiling:history.read",
  );
  const collaboratorId = added
    .split(/\s+/)
    .find((w) => /^[A-Za-z0-9_-]{43}$/.test(w));
  if (!collaboratorId) throw new Error(`no credential id in: ${added}`);
  const authz = path.join(RUNTIME, "authz.db");
  const activeGrants = () =>
    sql(
      authz,
      `SELECT group_concat(cap, ',') FROM (SELECT cap FROM grants WHERE cred_id = '${collaboratorId}' AND session_id = '${session}' AND revoked_at IS NULL ORDER BY cap)`,
    );
  await owner.mouseClick('button[aria-label="Actions"]');
  await owner.click("Share…");
  await owner.select("Approved key", collaboratorId);
  await owner.eval(
    `([...document.querySelectorAll('label')].find((l) => l.innerText.trim() === "Write to this session")?.querySelector('input'))?.click()`,
  );
  await owner.click("Allow selected access");
  await waitFor(
    "host recorded the session grant",
    () => activeGrants() === "history.read,session.write",
  );
  await waitFor("grant form resets after saving", () =>
    owner.eval(
      `document.querySelector('select[aria-label="Approved key"]')?.value === ''`,
    ),
  );
  await owner.select("Approved key", collaboratorId);
  await owner.click("Remove access");
  await waitFor("host removed the session grant", () => activeGrants() === "");
  step("key access granted and removed on the host", { collaboratorId });

  const beforePublish = {
    centralTranscript: centralHas(TRANSCRIPT_MARK),
    centralReply: centralHas(REPLY_MARK),
    // The session's folder is private metadata (spec: never uploaded).
    centralCwd: centralHas(path.join(HOST_HOME, "work")),
  };
  evidence.leaks.beforePublish = beforePublish;
  if (Object.values(beforePublish).some(Boolean)) {
    throw new Error(
      `private content reached central before publication: ${JSON.stringify(beforePublish)}`,
    );
  }

  // ---- publish prior + future history to ONE named reader ----
  const reader = await new Account(READER).register();
  const outsider = await new Account(OUTSIDER).register();
  // The Share dialog is already open: publication and key grants are one flow.
  await waitFor("publication status Private", () =>
    owner.eval(
      `document.querySelector('[data-publication-state]')?.innerText === 'Private'`,
    ),
  );
  await owner.type("Add people by email", READER);
  await sleep(500);
  await owner.press("Enter");
  await owner.waitText(READER);
  await owner.click("Share read-only");
  await waitFor("publication accepted", () =>
    owner.eval(
      `['Syncing', 'Shared'].includes(document.querySelector('[data-publication-state]')?.innerText)`,
    ),
  );
  const published = await waitFor(
    "reader sees the published prior history",
    async () => {
      const r = await reader.request("GET", `/api/sessions/${session}`);
      const body = await r.text();
      return r.status === 200 && body.includes(TRANSCRIPT_MARK) ? body : null;
    },
    60_000,
  );
  step("published prior history", { bytes: published.length });
  fs.writeFileSync(path.join(STUBS, `mutate.${session}`), "");
  await waitFor(
    "reader sees history written after publishing",
    async () => {
      const r = await reader.request("GET", `/api/sessions/${session}`);
      return r.status === 200 && (await r.text()).includes(MUTATION_MARK);
    },
    60_000,
  );
  step("published future history");
  const denied = await outsider.request("GET", `/api/sessions/${session}`);
  const deniedBody = await denied.text();
  if (denied.status === 200 || deniedBody.includes(TRANSCRIPT_MARK))
    throw new Error(`outsider read it: ${denied.status}`);
  step("outsider denied", { status: denied.status });

  // The reader's view in the built SPA, in its own browser context.
  const readerContext = (await cdp.send("Target.createBrowserContext"))
    .browserContextId;
  const readerPage = await cdp.page("reader", { context: readerContext });
  await readerPage.init();
  await readerPage.goto("/login");
  await readerPage.fetchJson("POST", "/auth/login", {
    email: READER,
    password: PASSWORD,
  });
  await readerPage.goto(`/sessions/${session}`);
  await readerPage.waitText(TRANSCRIPT_MARK, 60_000);
  const outsiderContext = (await cdp.send("Target.createBrowserContext"))
    .browserContextId;
  const outsiderPage = await cdp.page("outsider", { context: outsiderContext });
  await outsiderPage.init();
  await outsiderPage.goto("/login");
  await outsiderPage.fetchJson("POST", "/auth/login", {
    email: OUTSIDER,
    password: PASSWORD,
  });
  await outsiderPage.goto(`/sessions/${session}`);
  await sleep(3_000);
  const outsiderSees = (await outsiderPage.text()).includes(TRANSCRIPT_MARK);
  if (outsiderSees)
    throw new Error("outsider's SPA shows the published history");
  step("SPA: reader reads, outsider does not");

  // ---- host offline: the published copy is still readable ----
  await stopHost();
  await waitFor("relay shows host offline", async () => {
    const r = await owner.fetchJson("GET", `/api/host-relay/${hostId}/status`);
    return r.status === 200 && JSON.parse(r.body).online === false;
  });
  await readerPage.goto(`/sessions/${session}`);
  await readerPage.waitText(MUTATION_MARK, 60_000);
  step("offline published copy readable");

  // ---- unshare: deletion, links disabled, no stale resurrection ----
  await startHost();
  await waitFor("relay shows host online again", async () => {
    const r = await owner.fetchJson("GET", `/api/host-relay/${hostId}/status`);
    return r.status === 200 && JSON.parse(r.body).online === true;
  });
  await owner.goto(`/private/${session}`);
  await owner.showHistory();
  await owner.waitText(TRANSCRIPT_MARK, 60_000);
  await owner.mouseClick('button[aria-label="Actions"]');
  await owner.click("Share…");
  await waitFor(
    "publication status Shared",
    () =>
      owner.eval(
        `document.querySelector('[data-publication-state]')?.innerText === 'Shared'`,
      ),
    60_000,
  );
  // Reopened after a host restart, the dialog shows the audience the host
  // holds (from its signed status), not an owner-only default.
  const shownAudience = await waitFor("dialog shows the host's audience", () =>
    owner.eval(
      `(() => { const d = document.querySelector('[role=dialog]'); const r = d?.querySelector('[role=radio][aria-label="Specific people"]'); return d && r?.getAttribute('aria-checked') === 'true' && d.innerText.includes(${JSON.stringify(READER)}) ? d.innerText.match(/Host status:[^\\n]*/)?.[0] ?? 'shown' : null; })()`,
    ),
  );
  step("dialog shows the host's actual audience", { shownAudience });

  // ---- unshare while the host is offline: a visible failure, no change ----
  await stopHost();
  await waitFor("relay shows host offline for unshare", async () => {
    const r = await owner.fetchJson("GET", `/api/host-relay/${hostId}/status`);
    return r.status === 200 && JSON.parse(r.body).online === false;
  });
  await owner.click("Stop sharing");
  const offlineProblem = await waitFor(
    "the dialog says the unshare did not reach the host",
    () =>
      owner.eval(
        `document.querySelector('[role=dialog] [role=alert]')?.innerText || null`,
      ),
    120_000,
  );
  const whileOffline = await reader.request("GET", `/api/sessions/${session}`);
  if (whileOffline.status !== 200)
    throw new Error(
      `an unshare the host never received changed the published copy: ${whileOffline.status}`,
    );
  step("offline unshare failed visibly; published copy unchanged", {
    offlineProblem,
  });
  await startHost();
  await waitFor("relay shows host online to retry the unshare", async () => {
    const r = await owner.fetchJson("GET", `/api/host-relay/${hostId}/status`);
    return r.status === 200 && JSON.parse(r.body).online === true;
  });
  // The retry is the user's click; nothing resent it meanwhile.
  await sleep(3_000);
  const beforeRetry = await reader.request("GET", `/api/sessions/${session}`);
  if (beforeRetry.status !== 200)
    throw new Error("the failed unshare was retried without a click");
  // Reopened, the dialog reads the host again over a fresh private channel
  // (signaled while central is up): the offline attempt changed nothing.
  await owner.click("Close");
  await owner.mouseClick('button[aria-label="Actions"]');
  await owner.click("Share…");
  await waitFor(
    "after the failed offline unshare the host still shares",
    () =>
      owner.eval(
        `document.querySelector('[data-publication-state]')?.innerText === 'Shared'`,
      ),
    60_000,
  );
  step("the failed offline unshare left the host sharing");

  // ---- central unreachable, private channel up: the host takes the signed
  // unshare, says Deletion pending, and deletes first once central is back ----
  central.kill("SIGSTOP");
  let pendingWhileCentralDown;
  try {
    await owner.click("Stop sharing");
    const shown = await waitFor(
      "the host reports the unshare it accepted as Deletion pending",
      () =>
        owner.eval(
          `(() => { const s = document.querySelector('[data-publication-state]')?.innerText; return s === 'Deletion pending' ? s : document.querySelector('[role=dialog] [role=alert]')?.innerText ? 'problem: ' + document.querySelector('[role=dialog] [role=alert]').innerText : null; })()`,
        ),
      90_000,
    );
    if (shown !== "Deletion pending")
      throw new Error(`unshare with central down: ${shown}`);
    pendingWhileCentralDown = {
      stateShown: shown,
      // Central was never told: its copy is still there.
      centralSessionRows: sql(
        CENTRAL_DB,
        `SELECT COUNT(*) FROM sessions WHERE id = '${session}'`,
      ),
    };
    if (pendingWhileCentralDown.centralSessionRows !== "1")
      throw new Error(
        `central changed while stopped: ${JSON.stringify(pendingWhileCentralDown)}`,
      );
  } finally {
    central.kill("SIGCONT");
  }
  step(
    "central down: the host accepted the unshare, Deletion pending, central copy untouched",
    pendingWhileCentralDown,
  );
  await waitFor(
    "reader loses access once central is back",
    async () => {
      const r = await reader.request("GET", `/api/sessions/${session}`);
      return r.status !== 200;
    },
    120_000,
  );
  const unshareState = await waitFor("the host settles the unshare", () =>
    owner.eval(
      `(() => { const s = document.querySelector('[data-publication-state]')?.innerText; return ['Deletion pending', 'Private'].includes(s) ? s : null; })()`,
    ),
  );
  step("central back: the pending deletion ran first", {
    stateShown: unshareState,
  });
  fs.writeFileSync(path.join(STUBS, `after-unshare.${session}`), "");
  await waitFor(
    "local history has the post-unshare message",
    () =>
      sql(
        history,
        `SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH '${AFTER_UNSHARE_MARK}'`,
      ) !== "0",
    60_000,
  );
  // Reconnect: any stale sync would run now.
  await stopHost();
  await startHost();
  await waitFor("relay shows host online after reconnect", async () => {
    const r = await owner.fetchJson("GET", `/api/host-relay/${hostId}/status`);
    return r.status === 200 && JSON.parse(r.body).online === true;
  });
  await sleep(10_000);
  const after = await reader.request("GET", `/api/sessions/${session}`);
  await readerPage.goto(`/sessions/${session}`);
  await sleep(3_000);
  const resurrected = {
    readerStatus: after.status,
    readerSpaShowsTranscript: (await readerPage.text()).includes(
      TRANSCRIPT_MARK,
    ),
    centralSessionRows: sql(
      CENTRAL_DB,
      `SELECT COUNT(*) FROM sessions WHERE id = '${session}'`,
    ),
    centralAfterUnshareMark: centralHas(AFTER_UNSHARE_MARK),
    centralRowsWithTranscript: centralRowsHave(TRANSCRIPT_MARK),
    centralRowsWithFutureHistory: centralRowsHave(MUTATION_MARK),
    // Reported, not asserted: SQLite keeps deleted bytes in the WAL/free pages
    // until a checkpoint or VACUUM (an accepted administrator-copy limit).
    centralTranscriptBytesResidual: centralHas(TRANSCRIPT_MARK),
  };
  evidence.leaks.afterUnshare = resurrected;
  if (
    resurrected.readerStatus === 200 ||
    resurrected.readerSpaShowsTranscript ||
    resurrected.centralSessionRows !== "0" ||
    resurrected.centralAfterUnshareMark ||
    resurrected.centralRowsWithTranscript ||
    resurrected.centralRowsWithFutureHistory
  ) {
    throw new Error(`unshared copy came back: ${JSON.stringify(resurrected)}`);
  }
  step("no stale resurrection", resurrected);

  // ---- credential revoke: the followed terminal stops; unlock is visible
  // and refused; nothing comes back until a NEW passkey is approved ----
  await owner.goto(`/private/${session}`);
  await owner.showHistory();
  await owner.waitText(TRANSCRIPT_MARK, 60_000);
  await displayMenu();
  await owner.click("Fullscreen");
  await waitFor(
    "the terminal follows again before the revoke",
    async () => (await screenText()).includes(LEASE_AGAIN_MARK),
    60_000,
  );
  const activeDelegations = (cred) =>
    sql(
      authz,
      `SELECT COUNT(*) FROM delegations WHERE parent_cred = '${cred}' AND revoked_at IS NULL AND not_after > strftime('%s','now')`,
    );
  const delegationsBefore = activeDelegations(pendingId);
  hostAdmin("credential", "revoke", "--", pendingId);
  // The next frame meets the withdrawn authority.
  await say(REVOKED_MARK);
  await waitFor(
    "a visible unlock is offered",
    () => followButton("Unlock and follow again"),
    60_000,
  );
  await owner.click("Unlock and follow again");
  // The host refuses an unlock under the revoked passkey.
  await waitFor(
    "the refused unlock leaves the unlock offered",
    () =>
      owner.eval(
        `[...document.querySelectorAll('button')].some((b) => b.innerText.trim() === 'Unlock and follow again' && !b.disabled)`,
      ),
    60_000,
  );
  await sleep(5_000);
  const afterRefusedUnlock = {
    delegationsBefore,
    activeDelegations: activeDelegations(pendingId),
    credentialRevoked:
      sql(
        authz,
        `SELECT COUNT(*) FROM credentials WHERE cred_id = '${pendingId}' AND revoked_at IS NOT NULL`,
      ) === "1",
    screenShowsRevokedMark: (await screenText()).includes(REVOKED_MARK),
    unlockStillOffered: await followButton("Unlock and follow again"),
  };
  evidence.revoke = { afterRefusedUnlock };
  if (
    afterRefusedUnlock.activeDelegations !== "0" ||
    !afterRefusedUnlock.credentialRevoked ||
    afterRefusedUnlock.screenShowsRevokedMark ||
    !afterRefusedUnlock.unlockStillOffered
  )
    throw new Error(
      `a revoked passkey still reached the terminal: ${JSON.stringify(afterRefusedUnlock)}`,
    );
  step("revoked: terminal stopped, visible unlock refused", afterRefusedUnlock);

  // Regrant: only a NEW passkey, approved locally, follows again.
  const reStarted = hostAdmin(
    "passkey",
    "enroll-start",
    "--label",
    "phone-again",
    ...grants.flatMap((g) => ["--grant", g]),
  );
  const reLink = reStarted
    .split(/\s+/)
    .find((w) => w.startsWith(`${ORIGIN}/host-link#`));
  if (!reLink)
    throw new Error(`no enrollment link in: ${reStarted.slice(0, 400)}`);
  await owner.goto(reLink);
  await owner.waitText("Connect this device");
  const reRegister = await waitFor("register button again", () =>
    owner.eval(
      `[...document.querySelectorAll('button')].map((b) => b.innerText.trim()).find((t) => /passkey/i.test(t)) || null`,
    ),
  );
  await owner.click(reRegister);
  await waitFor(
    "second registration done",
    async () =>
      /approve|registered|created/i.test(await owner.text()) &&
      !/not valid/i.test(await owner.text()),
  );
  const rePendingId = JSON.parse(hostAdmin("passkey", "pending")).pending?.find(
    (p) => p.label === "phone-again",
  )?.cred_id;
  if (!rePendingId) throw new Error("no pending second registration");
  hostAdmin("passkey", "approve", "--", rePendingId);
  await owner.goto("/settings");
  await owner.click("Unlock with passkey");
  await owner.waitText("Unlocked until");
  await owner.goto(`/private/${session}`);
  await owner.showHistory();
  await owner.waitText(TRANSCRIPT_MARK, 60_000);
  await displayMenu();
  await owner.click("Fullscreen");
  await waitFor(
    "the new passkey follows the terminal again",
    async () => (await screenText()).includes(REVOKED_MARK),
    60_000,
  );
  const regranted = {
    oldActiveDelegations: activeDelegations(pendingId),
    newActiveDelegations: activeDelegations(rePendingId),
    oldCredentialRevoked:
      sql(
        authz,
        `SELECT COUNT(*) FROM credentials WHERE cred_id = '${pendingId}' AND revoked_at IS NOT NULL`,
      ) === "1",
  };
  evidence.revoke.regranted = regranted;
  if (
    regranted.oldActiveDelegations !== "0" ||
    regranted.newActiveDelegations === "0" ||
    !regranted.oldCredentialRevoked
  )
    throw new Error(
      `the revoked passkey came back: ${JSON.stringify(regranted)}`,
    );
  step("regranted only through a new, locally approved passkey", regranted);
  const turnOnCentral = {
    user: centralHas(TURN_USER),
    good: centralHas(TURN_GOOD),
    bad: centralHas(TURN_BAD),
  };
  if (Object.values(turnOnCentral).some(Boolean))
    throw new Error(
      `TURN settings reached central: ${JSON.stringify(turnOnCentral)}`,
    );
  const laterScreensOnCentral = {
    lease: centralHas(LEASE_MARK),
    leaseAgain: centralHas(LEASE_AGAIN_MARK),
    revoked: centralHas(REVOKED_MARK),
  };
  if (Object.values(laterScreensOnCentral).some(Boolean))
    throw new Error(
      `terminal bytes reached central: ${JSON.stringify(laterScreensOnCentral)}`,
    );
  step("central holds none of the later screens", laterScreensOnCentral);

  // ---- the browser never put private content on a central request ----
  const requests = await owner.requestBodies();
  const leaking = requests.filter((r) =>
    [
      REPLY_MARK,
      TRANSCRIPT_MARK,
      SCREEN_MARK,
      UPDATE_MARK,
      FILE_NAME,
      FILE_BODY_MARK,
      IMAGE_MARK,
      TURN_USER,
      TURN_GOOD,
      TURN_BAD,
      BAD_REPLY_MARK,
      LEASE_MARK,
      LEASE_AGAIN_MARK,
      REVOKED_MARK,
    ].some((m) => (r.url + (r.body ?? "")).includes(m)),
  );
  evidence.leaks.browserRequests = {
    total: requests.length,
    leaking: leaking.map((r) => `${r.method} ${r.url}`),
  };
  if (leaking.length)
    throw new Error(
      `browser sent private content to central: ${JSON.stringify(evidence.leaks.browserRequests)}`,
    );
  step("browser requests carry no private content", { total: requests.length });

  // What crossed the relay (readable by central) at all: only the passkey
  // bootstrap and channel signaling. Every signed control uses the channel.
  const RELAY_ALLOWED = [
    "enroll_start",
    "enroll_finish",
    "delegate_start",
    "delegate_finish",
    "peer_challenge",
    "peer.offer",
  ];
  const relayOps = requests
    .filter((r) => r.url.includes("/api/host-relay/") && r.body)
    .map((r) => {
      try {
        const body = JSON.parse(r.body);
        const op = (b) =>
          JSON.parse(Buffer.from(b.p, "base64url").toString("utf8")).op;
        // A signed control on the relay is never allowed, whatever it names.
        if (body.type === "signed_op") return `signed_op:${op(body)}`;
        if (body.type === "peer_offer") return op(body);
        return body.type;
      } catch {
        return "<unparsed>";
      }
    });
  evidence.relayOperations = relayOps;
  const misrouted = relayOps.filter((op) => !RELAY_ALLOWED.includes(op));
  if (misrouted.length)
    throw new Error(
      `more than bootstrap and signaling crossed the relay: ${misrouted.join(", ")}`,
    );
  step("relay carried only channel signaling and passkey bootstrap", {
    ops: [...new Set(relayOps)],
  });
  evidence.result = "passed";
} catch (error) {
  failure = error;
} finally {
  clearTimeout(watchdog);
  await finish(failure ? "failed" : "passed", failure);
}
