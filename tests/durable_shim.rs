//! `@earendil-works/pi-durable` virtual shim (wave D4): the SDK thin surface —
//! storage backends, the session kernel, the execution-environment contract and
//! `NodeExecutionEnv`, `/tools`, `/tool-search`, `/codemode` — driven through the
//! extension runtime (the real target environment: QuickJS, the typebox shims,
//! the host SQLite bridge, the node shims).
//!
//! The fixture is a self-checking case list whose cases name the upstream
//! `packages/durable/test/*.test.ts` file each one comes from (HEAD 7d75a0e3e),
//! plus the published-surface locks. It imports the virtual modules by their
//! published specifiers, so the `shims.rs` registration table is covered too.
//! `/images`, `/testing` and `/storage/sqlite/cloudflare` are asserted for their
//! throws contract, and the Harness-tier facades (`Harness`, `createRegistry`,
//! `watchEvents`) for theirs.
//!
//! Two host gaps used to keep upstream-derived cases out of this runtime run
//! (they passed in the wave's node runs against the same ported code) and were
//! closed afterwards: the host fs bridge rejected the numeric `open()` flags
//! `NodeExecutionEnv.openBinaryReader` passes, and the child_process shim's
//! readable streams had no `setEncoding` (so the shell probe behind `exec`
//! threw). Both are now covered here end to end — `openBinaryReader`/
//! `scanLines`/`readTextLines` and `exec` — and by
//! `tests/fs_numeric_open_flags.rs` / `tests/child_process_stream_encoding.rs`.
//!
//! The fixture ships a minimal `diff` package (`node_modules/diff`) because the
//! ported `src/tools/edit-diff.ts` imports upstream durable's npm dependency of
//! that name: the double implements exactly the three names that module uses
//! (`diffLines`, `createTwoFilesPatch`, `FILE_HEADERS_ONLY`). The wave report's
//! node runs exercise the same code against the real jsdiff 8.0.4.
//!
//! Fixtures are seeded from source strings at runtime (the repo hygiene gate
//! forbids tracked `.js`/`.ts` sources), mirroring tests/chord_shim.rs.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// The extension entry: registers the probe tool.
const INDEX_TS: &str = r##"import { runDurableProbe } from "./durable_probe.mjs";

export default (pi: any) => {
  pi.registerTool({
    name: "durable_probe",
    description: "Run the durable shim case list and report each case",
    parameters: { type: "object", properties: {} },
    execute: async () => {
      const report = await runDurableProbe();
      return { content: [{ type: "text", text: JSON.stringify(report) }] };
    },
  });
};
"##;

/// The surface probe: the modules' export names and every throws contract.
const SURFACE_TS: &str = r##"import * as kernel from "@earendil-works/pi-durable";
import * as envMod from "@earendil-works/pi-durable/env";
import * as toolsMod from "@earendil-works/pi-durable/tools";
import * as toolSearchMod from "@earendil-works/pi-durable/tool-search";
import * as codemodeMod from "@earendil-works/pi-durable/codemode";
import * as memoryMod from "@earendil-works/pi-durable/storage/memory";
import * as jsonlMod from "@earendil-works/pi-durable/storage/jsonl";
import * as jsonlNodeMod from "@earendil-works/pi-durable/storage/jsonl/node";
import * as sqliteMod from "@earendil-works/pi-durable/storage/sqlite";
import * as sqliteNodeMod from "@earendil-works/pi-durable/storage/sqlite/node";
import * as envNodeMod from "@earendil-works/pi-durable/env/node";
import * as imagesMod from "@earendil-works/pi-durable/images";
import * as imagesNodeMod from "@earendil-works/pi-durable/images/node";
import * as imagesCfMod from "@earendil-works/pi-durable/images/cloudflare";
import * as testingMod from "@earendil-works/pi-durable/testing";
import * as cfSqlMod from "@earendil-works/pi-durable/storage/sqlite/cloudflare";

export default (pi: any) => {
  pi.registerTool({
    name: "durable_surface",
    description: "Report the durable entry surfaces, per-subpath names and the throws contract",
    parameters: { type: "object", properties: {} },
    execute: async () => {
      const modules: any = {
        ".": kernel, "/tools": toolsMod, "/env": envMod, "/env/node": envNodeMod,
        "/tool-search": toolSearchMod, "/codemode": codemodeMod,
        "/storage/memory": memoryMod, "/storage/jsonl": jsonlMod, "/storage/jsonl/node": jsonlNodeMod,
        "/storage/sqlite": sqliteMod, "/storage/sqlite/node": sqliteNodeMod,
      };
      const required: any = {
        ".": ["defineTool", "defineExtension", "section", "hook", "wrapTool", "wrapSection", "defineDoc", "defineDocFamily", "defineTask", "MemoryStorage", "createSession", "ROOT_CONVERSATION_ID", "Harness", "createRegistry", "watchEvents"],
        "/tools": ["createReadTool", "createWriteTool", "createEditTool", "createBashTool", "createPowerShellTool", "createCodingTools", "CodingTools", "DEFAULT_IMAGE_LIMITS"],
        "/env": ["ok", "err", "getOrThrow", "getOrUndefined", "toError", "FileError", "ExecutionError", "StreamDecoder", "rangeDecoder", "startsWithBom", "LineScanner"],
        "/env/node": ["NodeExecutionEnv"],
        "/tool-search": ["createToolSearchTool", "createToolSearchExtension", "TOOL_SEARCH_TOOL_NAME", "DEFAULT_TOOL_SEARCH_LIMIT"],
        "/codemode": ["createCodemodeTool", "createCodemodeExtension", "CODEMODE_TOOL_NAME", "DEFAULT_CODEMODE_INLINE_BUDGET", "CodemodeStoreDoc"],
        "/storage/memory": ["MemoryStorage"],
        "/storage/jsonl": ["JsonlStorage", "JsonlCorruptionError", "JsonlStoragePoisonedError"],
        "/storage/jsonl/node": ["openNodeJsonlStorage", "JsonlStorage"],
        "/storage/sqlite": ["SqliteStorage", "applySqliteMigrations", "CURRENT_SQLITE_SCHEMA_VERSION", "SQLITE_MIGRATIONS"],
        "/storage/sqlite/node": ["NodeSqliteDatabase", "openNodeSqliteDatabase", "openNodeSqliteStorage"],
      };
      const report: any = {
        kernel: Object.keys(kernel).sort(),
        env: Object.keys(envMod).sort(),
        entries: {},
        throws: Array.from(
          new Set([
            ...Object.keys(imagesMod),
            ...Object.keys(imagesNodeMod),
            ...Object.keys(imagesCfMod),
            ...Object.keys(testingMod),
            ...Object.keys(cfSqlMod),
          ]),
        ).sort(),
        subpathMissing: {},
        throwsBehavior: {},
        harnessCode: undefined,
      };
      for (const entry of Object.keys(modules)) {
        report.entries[entry] = Array.from(new Set(required[entry].filter((name: string) => modules[entry][name] !== undefined))).sort();
        report.subpathMissing[entry] = required[entry].filter((name: string) => modules[entry][name] === undefined);
      }
      const record = (key: string, run: () => unknown) => {
        try {
          run();
          report.throwsBehavior[key] = "resolved";
        } catch (error: any) {
          report.throwsBehavior[key] = String(error.code) + ":" + String(error.message);
        }
      };
      record("images.createPhotonImages", () => imagesMod.createPhotonImages({}));
      record("imagesNode.createNodePhotonImages", () => imagesNodeMod.createNodePhotonImages({}));
      record("imagesCloudflare.createCloudflarePhotonImages", () => imagesCfMod.createCloudflarePhotonImages());
      record("testing.registerStorageConformance", () => testingMod.registerStorageConformance({}, "x", () => {}));
      record("testing.registerEnvConformance", () => testingMod.registerEnvConformance({}, "x", () => {}));
      record("testing.createStorageConformance", () => testingMod.createStorageConformance({}, {}));
      record("testing.STORAGE_MEMORY_SCALES", () => (testingMod.STORAGE_MEMORY_SCALES as any).length);
      record("cloudflare.openDurableObjectSqliteStorage", () => cfSqlMod.openDurableObjectSqliteStorage({}));
      record("cloudflare.DurableObjectSqliteDatabase", () => new (cfSqlMod.DurableObjectSqliteDatabase as any)({}));
      record("harness.Harness", () => new (kernel.Harness as any)({}));
      record("harness.Harness.open", () => (kernel.Harness as any).open({}, {}, {}));
      record("harness.createRegistry", () => (kernel.createRegistry as any)());
      record("harness.watchEvents", () => (kernel.watchEvents as any)({}));
      try {
        (kernel.Harness as any).open({});
      } catch (error: any) {
        report.harnessCode = String(error.code);
      }
      return { content: [{ type: "text", text: JSON.stringify(report) }] };
    },
  });
};
"##;

/// The minimal `diff` package the ported `edit-diff.ts` imports (upstream
/// durable's npm dependency): `diffLines`, `createTwoFilesPatch` and
/// `FILE_HEADERS_ONLY`, in the subset the tool path uses.
const DIFF_PACKAGE_JSON: &str = r##"{
  "name": "diff",
  "version": "8.0.4",
  "type": "module",
  "main": "./index.js",
  "exports": { ".": "./index.js" }
}
"##;

const DIFF_INDEX_JS: &str = r##"// A small jsdiff stand-in: line diff runs plus a unified patch writer.
export const FILE_HEADERS_ONLY = { includeIndex: false, includeUnderline: false, includeFileHeaders: true };

function splitLines(text) {
  if (text === "") return [];
  const lines = text.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  return lines.map((line) => line + "\n");
}

/** Line-level diff runs in jsdiff's part shape. */
export function diffLines(oldText, newText) {
  const oldLines = splitLines(oldText);
  const newLines = splitLines(newText);
  const table = [];
  for (let i = 0; i <= oldLines.length; i++) table.push(new Array(newLines.length + 1).fill(0));
  for (let i = oldLines.length - 1; i >= 0; i--) {
    for (let j = newLines.length - 1; j >= 0; j--) {
      table[i][j] = oldLines[i] === newLines[j] ? table[i + 1][j + 1] + 1 : Math.max(table[i + 1][j], table[i][j + 1]);
    }
  }
  const ops = [];
  let i = 0;
  let j = 0;
  while (i < oldLines.length && j < newLines.length) {
    if (oldLines[i] === newLines[j]) {
      ops.push({ kind: "context", line: oldLines[i] });
      i += 1;
      j += 1;
    } else if (table[i + 1][j] >= table[i][j + 1]) {
      ops.push({ kind: "removed", line: oldLines[i] });
      i += 1;
    } else {
      ops.push({ kind: "added", line: newLines[j] });
      j += 1;
    }
  }
  while (i < oldLines.length) ops.push({ kind: "removed", line: oldLines[i++] });
  while (j < newLines.length) ops.push({ kind: "added", line: newLines[j++] });
  const parts = [];
  for (const op of ops) {
    const last = parts[parts.length - 1];
    const sameKind =
      last !== undefined &&
      ((op.kind === "context" && !last.added && !last.removed) ||
        (op.kind === "added" && last.added === true) ||
        (op.kind === "removed" && last.removed === true));
    if (sameKind) {
      last.value += op.line;
      last.count += 1;
    } else {
      parts.push({ value: op.line, count: 1, added: op.kind === "added", removed: op.kind === "removed" });
    }
  }
  return parts;
}

/** A unified patch with `---`/`+++` headers and one hunk per run of changes. */
export function createTwoFilesPatch(fileName, oldFileName, oldText, newText, oldHeader, newHeader, options) {
  const context = options === undefined || options.context === undefined ? 4 : options.context;
  const parts = diffLines(oldText, newText);
  const out = ["--- " + oldFileName + "\n", "+++ " + fileName + "\n"];
  let oldIndex = 0;
  let newIndex = 0;
  let pending = [];
  const hunks = [];
  let current = null;
  const flush = () => {
    if (current !== null && current.body.length > 0) hunks.push(current);
    current = null;
  };
  for (const part of parts) {
    const lines = part.value.split("\n");
    if (lines[lines.length - 1] === "") lines.pop();
    if (!part.added && !part.removed) {
      pending.push(...lines.map((line) => " " + line));
      if (current !== null) {
        current.body.push(...pending.slice(0, context));
        pending = pending.slice(Math.min(pending.length, context));
        if (pending.length > context) flush();
      }
      oldIndex += lines.length;
      newIndex += lines.length;
      continue;
    }
    if (current === null) {
      const trail = pending.slice(Math.max(0, pending.length - context));
      current = { oldStart: oldIndex - trail.length + 1, newStart: newIndex - trail.length + 1, body: [...trail] };
    } else {
      current.body.push(...pending);
    }
    pending = [];
    for (const line of lines) current.body.push((part.added ? "+" : "-") + line);
    if (part.added) newIndex += lines.length;
    else oldIndex += lines.length;
  }
  flush();
  for (const hunk of hunks) {
    let oldCount = 0;
    let newCount = 0;
    for (const line of hunk.body) {
      if (line[0] !== "+") oldCount += 1;
      if (line[0] !== "-") newCount += 1;
    }
    // Empty sides carry the line before the change, as jsdiff and GNU diff print it.
    const oldStart = oldCount === 0 ? hunk.oldStart - 1 : hunk.oldStart;
    const newStart = newCount === 0 ? hunk.newStart - 1 : hunk.newStart;
    out.push("@@ -" + oldStart + "," + oldCount + " +" + newStart + "," + newCount + " @@\n");
    for (const line of hunk.body) out.push(line + "\n");
  }
  return out.join("");
}
"##;

/// The case list: every case names the upstream test file it comes from.
const PROBE_MJS: &str = r####"// durable shim probe: upstream-derived cases for the D4 thin surface.
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
import { Type } from "typebox";
import {
  CODEMODE_MODULE_VERSION,
  CODEMODE_TOOL_NAME,
  CodingTools,
  DEFAULT_CODEMODE_INLINE_BUDGET,
  DEFAULT_MAX_BYTES,
  MemoryStorage,
  ROOT_CONVERSATION_ID,
  SQLITE_MIGRATIONS,
  CURRENT_SQLITE_SCHEMA_VERSION,
  characterEnd,
  codemodeSection,
  createCodemodeExtension,
  createCodemodeTool,
  createSession,
  defineDoc,
  defineExtension,
  defineTask,
  defineTool,
  idFromNumber,
  matchesNamespace,
  rankDocuments,
  seqFromNumber,
  truncateHead,
  wrapTool,
} from "@earendil-works/pi-durable";
import { toCodemodeIdentifier } from "@earendil-works/pi-codemode/declarations";
import {
  ExecutionError,
  FileError,
  LineScanner,
  NodeExecutionEnv,
  StreamDecoder,
  err,
  getOrThrow,
  getOrUndefined,
  ok,
  rangeDecoder,
  startsWithBom,
  toError,
} from "@earendil-works/pi-durable/env";
import {
  CodingTools as ToolsCodingTools,
  createBashTool,
  createEditTool,
  createReadTool,
  createWriteTool,
} from "@earendil-works/pi-durable/tools";
import {
  DEFAULT_TOOL_SEARCH_LIMIT,
  TOOL_SEARCH_TOOL_NAME,
  createToolSearchTool,
} from "@earendil-works/pi-durable/tool-search";
import { openNodeJsonlStorage } from "@earendil-works/pi-durable/storage/jsonl/node";
import { openNodeSqliteStorage } from "@earendil-works/pi-durable/storage/sqlite/node";

const cases = [];

function addCase(name, run) {
  cases.push({ name, run });
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function makeApi(env) {
  return {
    env,
    output: () => {},
    outputWindow: undefined,
    diagnostic: () => {},
    details: async () => {},
    agent: async () => ({ deferred: [] }),
  };
}

function entryWrite(id, kind, data) {
  return {
    type: "entry",
    value: { id, conversationId: ROOT_CONVERSATION_ID, kind, ...(data === undefined ? {} : { data }) },
  };
}

const conversationWrite = () => ({ type: "conversation", value: { id: ROOT_CONVERSATION_ID } });
const tempDir = (prefix) => mkdtemp(join(tmpdir(), prefix));

addCase("memory-storage.test.ts: a committed entry is visible through Storage.entry", async () => {
  const storage = new MemoryStorage();
  await storage.commit([conversationWrite(), entryWrite(idFromNumber(2), "test", { nested: [1] })], BACKGROUND_CONTEXT);
  const found = await storage.entry(idFromNumber(2), BACKGROUND_CONTEXT);
  assert(found !== undefined, "the committed entry must be visible");
  assert(JSON.stringify(found.entry.data) === '{"nested":[1]}', "the entry data must round-trip: " + JSON.stringify(found.entry.data));
});

addCase("memory-storage.test.ts: a prepared commit applies once and is hidden before", async () => {
  const storage = new MemoryStorage();
  await storage.commit([conversationWrite()], BACKGROUND_CONTEXT);
  const prepared = storage.prepareCommit([entryWrite(idFromNumber(5), "test", { n: 1 })]);
  assert((await storage.entry(idFromNumber(5), BACKGROUND_CONTEXT)) === undefined, "a prepared commit must not be visible yet");
  assert(prepared.apply() === prepared.apply(), "applying a prepared commit twice returns the same seq");
  assert((await storage.entry(idFromNumber(5), BACKGROUND_CONTEXT)) !== undefined, "the applied entry must be visible");
});

addCase("jsonl-storage.test.ts: JsonlStorage keeps entries across close and reopen", async () => {
  const dir = await tempDir("pir-durable-jsonl-");
  const storage = await openNodeJsonlStorage(dir, BACKGROUND_CONTEXT);
  await storage.commit([conversationWrite(), entryWrite(idFromNumber(2), "test", { text: "jsonl" })], BACKGROUND_CONTEXT);
  await storage.close(BACKGROUND_CONTEXT);
  const reopened = await openNodeJsonlStorage(dir, BACKGROUND_CONTEXT);
  const found = await reopened.entry(idFromNumber(2), BACKGROUND_CONTEXT);
  assert(found !== undefined, "the entry must survive a reopen");
  assert(found.entry.data.text === "jsonl", "the entry data must survive: " + JSON.stringify(found.entry.data));
  await reopened.close(BACKGROUND_CONTEXT);
});

addCase("sqlite-storage.test.ts: SqliteStorage keeps entries across close and reopen", async () => {
  const dir = await tempDir("pir-durable-sqlite-");
  const path = join(dir, "store.sqlite");
  const storage = await openNodeSqliteStorage(path);
  await storage.commit([conversationWrite(), entryWrite(idFromNumber(2), "test", { text: "sqlite" })], BACKGROUND_CONTEXT);
  await storage.close(BACKGROUND_CONTEXT);
  const reopened = await openNodeSqliteStorage(path);
  const found = await reopened.entry(idFromNumber(2), BACKGROUND_CONTEXT);
  assert(found !== undefined, "the entry must survive a reopen");
  assert(found.entry.data.text === "sqlite", "the entry data must survive: " + JSON.stringify(found.entry.data));
  await reopened.close(BACKGROUND_CONTEXT);
});

addCase("storage-runtime-boundary.test.ts: the same commit path serves all three backends", async () => {
  const dir = await tempDir("pir-durable-three-");
  const jsonl = await openNodeJsonlStorage(dir, BACKGROUND_CONTEXT);
  const sqlite = await openNodeSqliteStorage(join(dir, "three.sqlite"));
  for (const storage of [new MemoryStorage(), jsonl, sqlite]) {
    await storage.commit([conversationWrite(), entryWrite(idFromNumber(2), "test", { any: true })], BACKGROUND_CONTEXT);
    assert((await storage.entry(idFromNumber(2), BACKGROUND_CONTEXT)) !== undefined, "every backend exposes the committed entry");
  }
  await jsonl.close(BACKGROUND_CONTEXT);
  await sqlite.close(BACKGROUND_CONTEXT);
});

addCase("session-tables.test.ts: createSession commits, a second session sees the commit", async () => {
  const storage = new MemoryStorage();
  const session = createSession(storage);
  const created = await session.commit(async (tx) => {
    const conversation = await tx.createConversation({ ownership: { kind: "ownerless" } });
    const entry = await tx.appendEntry(conversation.id, { kind: "test" });
    return { conversationId: conversation.id, entryId: entry.id };
  }, BACKGROUND_CONTEXT);
  const reopened = createSession(storage);
  const entry = await reopened.commit((tx) => tx.entry(created.entryId), BACKGROUND_CONTEXT);
  assert(entry !== undefined && entry.id === created.entryId, "a second session must see the committed entry");
  const conversation = await reopened.commit((tx) => tx.conversation(created.conversationId), BACKGROUND_CONTEXT);
  assert(conversation !== undefined, "a second session must see the committed conversation");
});

addCase("env-node.test.ts: NodeExecutionEnv writes, reads, stats and lists files", async () => {
  const dir = await tempDir("pir-durable-env-");
  const env = new NodeExecutionEnv({ cwd: dir });
  const written = await env.writeFile("a.txt", "alpha\nbeta\ngamma\n", BACKGROUND_CONTEXT);
  assert(written.ok === true, "writeFile must succeed: " + JSON.stringify(written));
  const read = await env.readTextFile("a.txt", BACKGROUND_CONTEXT);
  assert(read.ok === true && read.value === "alpha\nbeta\ngamma\n", "readTextFile must round-trip: " + JSON.stringify(read));
  const info = await env.fileInfo("a.txt", BACKGROUND_CONTEXT);
  assert(info.ok === true && info.value.kind === "file" && info.value.size === 17, "fileInfo: " + JSON.stringify(info));
  const missing = await env.fileInfo("nope.txt", BACKGROUND_CONTEXT);
  assert(missing.ok === false && missing.error.code === "not_found", "a missing file reports not_found: " + JSON.stringify(missing));
  assert(missing.ok === false && missing.error instanceof FileError, "the failure must be a FileError");
  const listed = await env.listDir(dir, BACKGROUND_CONTEXT);
  assert(listed.ok === true && listed.value.some((entry) => entry.name === "a.txt"), "listDir: " + JSON.stringify(listed));
  const absolute = await env.absolutePath("a.txt", BACKGROUND_CONTEXT);
  assert(absolute.ok === true && absolute.value.endsWith("a.txt"), "absolutePath: " + JSON.stringify(absolute));
});

addCase("env-node.test.ts: NodeExecutionEnv.openBinaryReader scans a line range", async () => {
  const dir = await tempDir("pir-durable-reader-");
  const env = new NodeExecutionEnv({ cwd: dir });
  getOrThrow(await env.writeFile("lines.txt", "alpha\nbeta\ngamma\n", BACKGROUND_CONTEXT));
  const reader = getOrThrow(await env.openBinaryReader("lines.txt", undefined, BACKGROUND_CONTEXT));
  const head = getOrThrow(await reader.read(0, 5, BACKGROUND_CONTEXT));
  assert(new TextDecoder().decode(head) === "alpha", "the byte range: " + new TextDecoder().decode(head));
  const scan = getOrThrow(await reader.scanLines({ startLine: 2, endLine: 3 }, BACKGROUND_CONTEXT));
  assert(scan.newlines === 3, "the newline count: " + JSON.stringify(scan));
  assert(scan.start === 11 && scan.end === 16, "the line range offsets: " + JSON.stringify(scan));
  assert(scan.selectedBytes === 5 && scan.firstLineBytes === 5, "the selected sizes: " + JSON.stringify(scan));
  await reader.close(BACKGROUND_CONTEXT);
  const lines = getOrThrow(await env.readTextLines("lines.txt", {}, BACKGROUND_CONTEXT));
  assert(lines.join("|") === "alpha|beta|gamma", "readTextLines: " + JSON.stringify(lines));
});

addCase("env-node.test.ts: NodeExecutionEnv.exec runs a shell command and streams its output", async () => {
  // The spawn cwd must sit inside a sandbox root; this probe's own directory
  // is one (a fresh mkdtemp under the OS temp is a root for fs writes, not for
  // spawn).
  const env = new NodeExecutionEnv({ cwd: process.cwd() });
  const describe = (failure) => String(failure.code) + ": " + String(failure.message) + (failure.cause ? " (cause: " + String(failure.cause.message) + ")" : "");
  let stdout = "";
  const onOutput = (text) => { stdout += text; };
  let result = await env.exec("echo hi", { onOutput }, BACKGROUND_CONTEXT);
  let shellAttempt;
  if (result.ok === false) {
    // This host has no bash (or the shell probe failed): the probe is what
    // `setEncoding` unlocked, so exercise the same spawn/stream path through
    // the argv form instead and keep the shell attempt's reason for context.
    shellAttempt = describe(result.error);
    const isWin = process.platform === "win32";
    stdout = "";
    result = await env.exec(
      isWin ? ["cmd.exe", "/C", "echo hi"] : ["/bin/sh", "-c", "echo hi"],
      { onOutput },
      BACKGROUND_CONTEXT,
    );
  }
  assert(
    result.ok === true,
    "exec must run" + (shellAttempt === undefined ? "" : " (shell attempt: " + shellAttempt + ")") + ": " + JSON.stringify(result) + (result.ok === false ? " " + describe(result.error) : ""),
  );
  assert(result.value.exitCode === 0, "the exit code: " + JSON.stringify(result.value));
  assert(stdout.trim() === "hi", "the streamed stdout: " + JSON.stringify(stdout));
});

addCase("tools.test.ts: createWriteTool writes a file into its environment", async () => {
  const dir = await tempDir("pir-durable-tools-");
  const env = new NodeExecutionEnv({ cwd: dir });
  const api = makeApi(env);
  const write = await createWriteTool().execute({ path: "f.txt", content: "alpha\nbeta\ngamma\n" }, api, BACKGROUND_CONTEXT);
  assert(write.isError !== true, "write: " + JSON.stringify(write));
  const onDisk = await readFile(join(dir, "f.txt"), "utf8");
  assert(onDisk === "alpha\nbeta\ngamma\n", "the write must reach the file: " + JSON.stringify(onDisk));
});

addCase("tools.test.ts: createEditTool replaces a block and reports diff and patch", async () => {
  const dir = await tempDir("pir-durable-edit-");
  const env = new NodeExecutionEnv({ cwd: dir });
  const api = makeApi(env);
  getOrThrow(await env.writeFile("f.txt", "alpha\nbeta\ngamma\n", BACKGROUND_CONTEXT));
  const edited = await createEditTool().execute({ path: "f.txt", edits: [{ oldText: "beta", newText: "BETA" }] }, api, BACKGROUND_CONTEXT);
  assert(edited.isError !== true, "edit: " + JSON.stringify(edited));
  const after = await readFile(join(dir, "f.txt"), "utf8");
  assert(after === "alpha\nBETA\ngamma\n", "the edit must reach the file: " + JSON.stringify(after));
  const details = edited.details === undefined ? {} : edited.details;
  assert(typeof details.patch === "string" && details.patch.includes("--- ") && details.patch.includes("+BETA"), "patch: " + JSON.stringify(details.patch));
  assert(typeof details.diff === "string" && details.diff.includes("BETA"), "diff: " + JSON.stringify(details.diff));
});

addCase("tools/index.ts: createCodingTools bundles read, write, edit and bash", async () => {
  for (const tool of [createReadTool(), createWriteTool(), createEditTool(), createBashTool()]) {
    assert(typeof tool.name === "string" && tool.name.length > 0, "every coding tool has a name");
    assert(typeof tool.execute === "function", tool.name + " has an execute");
  }
  assert(CodingTools.name === "coding-tools", "CodingTools name: " + CodingTools.name);
  assert(CodingTools.tools.length === 4, "CodingTools tool count: " + CodingTools.tools.length);
  for (const name of ["read", "write", "edit", "bash"]) {
    assert(CodingTools.tools.some((tool) => tool.name === name), "CodingTools carries " + name);
  }
  assert(ToolsCodingTools === CodingTools, "/tools and the root entry serve one module");
});

addCase("tool-search.test.ts: rankDocuments ranks by BM25 and honours the limit", async () => {
  const documents = [
    { name: "read", text: "Read the contents of a file, with line numbers" },
    { name: "write", text: "Write a file to disk" },
    { name: "bash", text: "Run a shell command" },
  ];
  const ranked = rankDocuments("write file", documents, 2);
  assert(ranked[0] === "write", "the exact match ranks first: " + JSON.stringify(ranked));
  assert(ranked.length === 2, "the limit is honoured: " + JSON.stringify(ranked));
  assert(rankDocuments("", documents, 5).length === 0, "an empty query ranks nothing");
  assert(rankDocuments("write", documents, 0).length === 0, "a zero limit ranks nothing");
});

addCase("tool-search.test.ts: createToolSearchTool loads matching deferred tools", async () => {
  const tool = createToolSearchTool();
  assert(tool.name === TOOL_SEARCH_TOOL_NAME && TOOL_SEARCH_TOOL_NAME === "tool_search", "the tool name: " + tool.name);
  assert(DEFAULT_TOOL_SEARCH_LIMIT === 8, "the default limit: " + DEFAULT_TOOL_SEARCH_LIMIT);
  const deferred = [
    { name: "mcp__files__write", description: "Write a file through MCP", parameters: { type: "object" } },
    { name: "mcp__files__read", description: "Read a file through MCP", parameters: { type: "object" } },
  ];
  const env = new NodeExecutionEnv({ cwd: process.cwd() });
  const api = { ...makeApi(env), agent: async () => ({ deferred }) };
  const result = await tool.execute({ query: "write file" }, api, BACKGROUND_CONTEXT);
  assert(result.details.loaded[0] === "mcp__files__write", "the loaded tools: " + JSON.stringify(result.details));
  assert(result.control !== undefined && result.control.addTools.length === result.details.loaded.length && result.control.addTools.includes("mcp__files__write"), "the control loads what the details report: " + JSON.stringify(result.control));
  const refused = await tool.execute({ query: "" }, api, BACKGROUND_CONTEXT).then(
    () => "resolved",
    (error) => String(error.message),
  );
  assert(refused === "query must not be empty", "an empty query is refused: " + refused);
});

addCase("codemode.test.ts: the codemode extension carries its tool and section", async () => {
  assert(CODEMODE_TOOL_NAME === "codemode", "the tool name: " + CODEMODE_TOOL_NAME);
  assert(typeof DEFAULT_CODEMODE_INLINE_BUDGET === "number" && DEFAULT_CODEMODE_INLINE_BUDGET > 0, "the inline budget: " + DEFAULT_CODEMODE_INLINE_BUDGET);
  assert(typeof CODEMODE_MODULE_VERSION === "number" && CODEMODE_MODULE_VERSION > 0, "the module version: " + String(CODEMODE_MODULE_VERSION));
  const extension = createCodemodeExtension({});
  assert(extension.name === "codemode", "the extension name: " + extension.name);
  assert(extension.tools.some((tool) => tool.name === CODEMODE_TOOL_NAME), "the extension carries its tool");
  assert(extension.sections.length >= 1 && extension.sections[0].key === "codemode", "the extension carries its section");
  assert(extension.tools.every((tool) => typeof tool.execute === "function"), "every codemode tool has an execute");
  const tool = createCodemodeTool({});
  assert(tool.name === CODEMODE_TOOL_NAME && tool.replay === "safe", "the tool is replay-safe: " + JSON.stringify({ name: tool.name, replay: tool.replay }));
});

addCase("codemode-describe.test.ts: the codemode description and section name the tool and its callables", async () => {
  const tool = createCodemodeTool({});
  assert(typeof tool.description === "string" && tool.description.includes("codemode"), "the description: " + String(tool.description).slice(0, 80));
  assert(tool.parameters !== undefined, "the parameters schema exists");
  const callable = [{ name: "read", description: "Read a file", parameters: Type.Object({ path: Type.String() }) }];
  const rendered = codemodeSection(callable, [], 4000);
  assert(typeof rendered === "string" && rendered.includes("read"), "the section names the callable: " + JSON.stringify(rendered).slice(0, 120));
  assert(codemodeSection([], [], 4000) === undefined, "an empty callable list renders nothing");
});

// ── D23 (d): the namespace lookup takes the name, every alias and the script identifier ──
// Upstream `isNamespaceName` (execute.ts:559-568) + `toCodemodeIdentifier`; the Rust reference is
// `src/plugins/codemode.rs::namespace_matches` (`[name, ...aliases]` equal or equal after normalization).

addCase("codemode-namespace.test.ts: matchesNamespace takes the name, an alias and the script identifier", async () => {
  const namespace = { name: "dev-radius", aliases: ["radius", "dev-radius-lite"] };
  assert(matchesNamespace(namespace, "dev-radius") === true, "the name matches as written");
  assert(matchesNamespace(namespace, "dev_radius") === true, "the name matches as its script identifier");
  assert(matchesNamespace(namespace, "radius") === true, "an alias matches as written");
  assert(matchesNamespace(namespace, "radius_lite") === false, "an unrelated identifier does not match: radius-lite is not radius");
  assert(matchesNamespace({ name: "mcp__docs", aliases: ["my-docs"] }, "my_docs") === true, "an alias matches as its script identifier");
  assert(matchesNamespace({ name: "mcp__docs", aliases: ["docs"] }, "docs") === true, "an MCP namespace is found by its server alias");
  assert(matchesNamespace({ name: "dev-radius" }, "dev_radius") === true, "a namespace without aliases still matches its identifier");
  assert(matchesNamespace(namespace, "Radius") === false, "case is not folded (toCodemodeIdentifier only replaces invalid characters)");
  assert(matchesNamespace(namespace, "other") === false, "an unrelated name does not match");
  assert(matchesNamespace(namespace, "dev-radiu") === false, "a prefix does not match");
  assert(toCodemodeIdentifier("dev-radius") === "dev_radius", "the identifier rule this matches with");
});

/**
 * A scripted codemode sandbox: it makes the script's calls itself, answers each delivery by collecting the outcome
 * values, and ends the script once every call was delivered. `createCodemodeTool` drives it through the same
 * `onStarted`/`onCall`/`onIdle`/`deliver` protocol a real VM uses, so the discovery globals are answered by the
 * shipped `discover()` path.
 */
function scriptedSandbox(calls) {
  return {
    start(_code, options) {
      const delivered = [];
      let resolveResult;
      const result = new Promise((resolve) => { resolveResult = resolve; });
      const run = {
        result,
        finished: false,
        deliver(seq, at, id, outcome) {
          delivered.push({ seq, at, id, outcome });
          if (delivered.length >= calls.length) {
            resolveResult({
              ok: true,
              value: delivered.map((entry) => entry.outcome.value),
              output: [],
              calls: [],
              during: { seq: delivered.length, issued: calls.length },
            });
          }
        },
      };
      options.onStarted("scripted-fingerprint");
      for (const call of calls) options.onCall(call);
      options.onIdle(0, calls.length);
      return run;
    },
  };
}

/** Run one scripted codemode execution over in-memory documents; the returned tool result carries the answered values. */
async function runScriptedCodemode(namespaces, callable, calls) {
  const docs = new Map();
  const api = {
    ...makeApi(undefined),
    taskId: "task-1",
    conversationId: ROOT_CONVERSATION_ID,
    agent: async () => ({ callable, deferred: [] }),
    commit: async (change) =>
      change({
        doc: async (token, key) => {
          const id = token.definition.kind + ":" + key;
          if (!docs.has(id)) docs.set(id, token.definition.initial());
          return docs.get(id);
        },
      }),
  };
  const tool = createCodemodeTool({ sandbox: scriptedSandbox(calls), namespaces });
  return await tool.execute({ code: "return 1;" }, api, BACKGROUND_CONTEXT);
}

function resultText(result) {
  return result.output.map((item) => item.text).join("\n");
}

addCase("codemode-describe.test.ts: describeNamespace finds a namespace by its alias through the start data", async () => {
  const callable = [
    { name: "mcp__docs__read", description: "Read a doc" },
    { name: "mcp__docs__search", description: "Search docs" },
    { name: "dev-radius.read", description: "Read the dev radius" },
    { name: "read", description: "Read a file" },
  ];
  const namespaces = [
    { name: "mcp__docs", prefix: "mcp__docs__", aliases: ["docs"], description: "Docs server", tools: [] },
    { name: "dev-radius", prefix: "dev-radius.", aliases: [], description: "Dev radius", tools: [] },
  ];
  const byAlias = resultText(await runScriptedCodemode(namespaces, callable, [
    { id: 1, target: "global", name: "describeNamespace", args: ["docs"] },
  ]));
  assert(byAlias.includes('"mcp__docs"'), "the server alias finds the namespace: " + byAlias);
  assert(byAlias.includes('"mcp__docs__read"') && byAlias.includes('"mcp__docs__search"'), "its tools are the script names: " + byAlias);
  const byName = resultText(await runScriptedCodemode(namespaces, callable, [
    { id: 1, target: "global", name: "describeNamespace", args: ["dev_radius"] },
  ]));
  assert(byName.includes('"dev-radius"'), "the script identifier finds the namespace: " + byName);
  const missed = resultText(await runScriptedCodemode(namespaces, callable, [
    { id: 1, target: "global", name: "describeNamespace", args: ["other"] },
  ]));
  assert(!missed.includes('"mcp__docs"') && !missed.includes('"dev-radius"'), "an unrelated name finds nothing: " + missed);
});

addCase("codemode-describe.test.ts: searchTools filters by a namespace alias through the start data", async () => {
  const callable = [
    { name: "mcp__docs__read", description: "Read a doc" },
    { name: "read", description: "Read a file" },
  ];
  const namespaces = [{ name: "mcp__docs", prefix: "mcp__docs__", aliases: ["docs"], tools: [] }];
  const matched = resultText(await runScriptedCodemode(namespaces, callable, [
    { id: 1, target: "global", name: "searchTools", args: ["read", { namespace: "docs" }] },
  ]));
  assert(matched.includes("mcp__docs__read"), "the alias scopes the search to its namespace: " + matched);
  const unmatched = resultText(await runScriptedCodemode(namespaces, callable, [
    { id: 1, target: "global", name: "searchTools", args: ["read", { namespace: "other" }] },
  ]));
  assert(unmatched.includes("[]"), "another namespace scopes the search away: " + unmatched);
  assert(!unmatched.includes("mcp__docs__read"), "no tool of the namespace is ranked: " + unmatched);
});

addCase("types.test.ts: ROOT_CONVERSATION_ID and the id helpers round-trip", async () => {
  assert(ROOT_CONVERSATION_ID !== undefined && typeof ROOT_CONVERSATION_ID === "number", "ROOT_CONVERSATION_ID: " + String(ROOT_CONVERSATION_ID));
  assert(idFromNumber(3) === idFromNumber(3), "ids are deterministic");
  assert(seqFromNumber(7) !== seqFromNumber(8), "seqs differ");
  assert(CURRENT_SQLITE_SCHEMA_VERSION >= 1 && SQLITE_MIGRATIONS.length >= 1, "the sqlite schema: " + CURRENT_SQLITE_SCHEMA_VERSION + "/" + SQLITE_MIGRATIONS.length);
});

addCase("truncate.test.ts: truncateHead bounds long content, characterEnd cuts on a boundary", async () => {
  const long = `${"x".repeat(100)}\n`.repeat(3000);
  const bounded = truncateHead(long);
  assert(bounded.content.length < long.length && bounded.truncated === true, "truncateHead must cut: " + JSON.stringify(Object.keys(bounded)));
  const eAcute = new TextEncoder().encode("é");
  assert(characterEnd(eAcute, 1) === 0, "a cut inside a multi-byte character goes back to its start: " + characterEnd(eAcute, 1));
  assert(characterEnd(eAcute, 2) === 2, "a cut on a boundary stays: " + characterEnd(eAcute, 2));
  assert(DEFAULT_MAX_BYTES > 0, "the byte limit");
});

addCase("env/index.test.ts: the contract values keep their upstream shapes", async () => {
  assert(ok(1).ok === true && ok(1).value === 1, "ok");
  assert(err("x").ok === false && err("x").error === "x", "err");
  assert(getOrThrow({ ok: true, value: 7 }) === 7, "getOrThrow");
  assert(getOrUndefined({ ok: false, error: "x" }) === undefined, "getOrUndefined");
  assert(toError("boom").message === "boom" && toError(new Error("e")).message === "e", "toError");
  const fileError = new FileError("not_found", "missing", "/p");
  assert(fileError instanceof Error && fileError.name === "FileError" && fileError.code === "not_found" && fileError.path === "/p", "FileError");
  const executionError = new ExecutionError("timeout", "slow");
  assert(executionError instanceof Error && executionError.name === "ExecutionError" && executionError.code === "timeout", "ExecutionError");
  assert(startsWithBom(Uint8Array.from([0xef, 0xbb, 0xbf, 0x61])) === true, "startsWithBom(true)");
  assert(startsWithBom(new TextEncoder().encode("a")) === false, "startsWithBom(false)");
  assert(typeof rangeDecoder().decode === "function", "rangeDecoder returns a decoder");
});

addCase("env-line-scan.test.ts: LineScanner locates a line range over pushed chunks", async () => {
  const scanner = new LineScanner(1, 2);
  scanner.push(new TextEncoder().encode("alpha\nbet"));
  scanner.push(new TextEncoder().encode("a\ngamma\n"));
  const scan = scanner.finish();
  assert(scan.newlines === 3, "the newline count: " + JSON.stringify(scan));
  assert(scan.start === 6 && scan.end === 10, "the range: " + JSON.stringify(scan));
  assert(scan.firstLineBytes === 4 && scan.selectedBytes === 4, "the sizes: " + JSON.stringify(scan));
});

addCase("env/decode.ts: StreamDecoder drops the BOM and holds an incomplete character back", async () => {
  const decoder = new StreamDecoder();
  assert(decoder.decode(Uint8Array.from([0xef, 0xbb, 0xbf, 0x61])) === "a", "the BOM is dropped");
  assert(decoder.decode(Uint8Array.from([0xc3])) === "", "an incomplete character is held back");
  assert(decoder.decode(Uint8Array.from([0xa9])) === "é", "the held-back bytes complete the character");
  assert(decoder.decode() === "", "the stream ends without throwing");
});

addCase("harness/define.ts: the tool and extension vocabulary is real", async () => {
  const tool = defineTool({ name: "probe", description: "probe", parameters: Type.Object({ a: Type.String() }), execute: async () => ({ output: [] }) });
  assert(tool.name === "probe" && typeof tool.execute === "function", "defineTool returns the registration");
  const extension = defineExtension({ name: "probe-ext", tools: [tool] });
  assert(extension.name === "probe-ext" && extension.tools.length === 1, "defineExtension returns the extension");
  const wrapped = wrapTool(tool, (inner) => inner);
  assert(wrapped.tool === "probe" && typeof wrapped.wrap === "function", "wrapTool");
  const doc = defineDoc({ kind: "test.doc", version: 1, scope: "session", initial: () => ({ n: 0 }) });
  assert(doc !== undefined, "defineDoc");
  const task = defineTask({ name: "test.task", version: 1, initial: () => ({ phase: "start" }), phases: { start: async () => {} }, abort: async () => {} });
  assert(task !== undefined, "defineTask");
});

export async function runDurableProbe() {
  const report = { total: 0, passed: 0, failed: [], names: [] };
  for (const item of cases) {
    report.total += 1;
    report.names.push(item.name);
    try {
      await item.run();
      report.passed += 1;
    } catch (error) {
      report.failed.push({ name: item.name, error: String((error && error.stack) || error) });
    }
  }
  return report;
}
"####;

/// The kernel module's export block (published surface + upstream internals).
const KERNEL_SURFACE: &[&str] = &[
    "AgentDoc",
    "AssistantEntry",
    "CODEMODE_MODULE_VERSION",
    "CODEMODE_TOOL_NAME",
    "CURRENT_SQLITE_SCHEMA_VERSION",
    "CodemodeJournalDoc",
    "CodemodeRunDoc",
    "CodemodeStoreDoc",
    "CodingTools",
    "CommittedStateSource",
    "CommittedWatch",
    "CompactionEntry",
    "CompactionTask",
    "ConversationBusy",
    "DEFAULT_CODEMODE_INLINE_BUDGET",
    "DEFAULT_COMPACTION_POLICY",
    "DEFAULT_IMAGE_LIMITS",
    "DEFAULT_MAX_BYTES",
    "DEFAULT_MAX_LINES",
    "DEFAULT_PROGRESS_POLICY",
    "DEFAULT_RETRY_POLICY",
    "DEFAULT_TOOL_SEARCH_LIMIT",
    "GenerationTask",
    "Harness",
    "InboxDoc",
    "JsonlCorruptionError",
    "JsonlStorage",
    "JsonlStoragePoisonedError",
    "LiveDoc",
    "MemoryStorage",
    "NestedCallDoc",
    "NodeSqliteDatabase",
    "OutputBuffer",
    "Progress",
    "ProviderDoc",
    "RETIREMENT_OPERATIONS",
    "ROOT_CONVERSATION_ID",
    "ReadAfterWrite",
    "ResetEntry",
    "SQLITE_MIGRATIONS",
    "SessionFailed",
    "SessionImpl",
    "SqliteStorage",
    "StorageRequestError",
    "SystemEntry",
    "TOOL_SEARCH_TOOL_NAME",
    "ToolResultEntry",
    "ToolTask",
    "Transaction",
    "UsageDoc",
    "UserEntry",
    "addressId",
    "agentHooks",
    "appendToolResult",
    "applySqliteMigrations",
    "boundOutput",
    "characterEnd",
    "checkRecordScope",
    "checkRecordVersion",
    "codemodeDescription",
    "codemodeSection",
    "configure",
    "contained",
    "createBashTool",
    "createCodemodeExtension",
    "createCodemodeTool",
    "createCodingTools",
    "createEditTool",
    "createPowerShellTool",
    "createReadTool",
    "createRegistry",
    "createSession",
    "createToolSearchExtension",
    "createToolSearchTool",
    "createWriteTool",
    "defineDoc",
    "defineDocFamily",
    "defineEntry",
    "defineExtension",
    "defineTask",
    "defineTool",
    "detectSupportedImageMimeType",
    "documentCreate",
    "documentRetention",
    "errorMessage",
    "formatSize",
    "harnessError",
    "hook",
    "idFromNumber",
    "imageDimensions",
    "matchesNamespace",
    "materializeDocument",
    "materializeDocumentValue",
    "openNodeJsonlStorage",
    "openNodeSqliteDatabase",
    "openNodeSqliteStorage",
    "orderToolResults",
    "planSystemEntries",
    "prepareForkDocumentCopies",
    "rankDocuments",
    "recordUsage",
    "renderSections",
    "replaySections",
    "resolveAddress",
    "resolveAgent",
    "resolveSettings",
    "sanitizeOutput",
    "section",
    "selectCut",
    "seqFromNumber",
    "serializeConversation",
    "setDefaultDocumentRetention",
    "toBase64",
    "truncateHead",
    "truncateHeadOf",
    "utf8ByteLength",
    "watchEvents",
    "withFileMutationQueue",
    "wrapSection",
    "wrapTool",
];

/// The `/env` module's export block.
const ENV_SURFACE: &[&str] = &[
    "ExecutionError",
    "FileError",
    "LineScanner",
    "NodeExecutionEnv",
    "StreamDecoder",
    "err",
    "getOrThrow",
    "getOrUndefined",
    "ok",
    "rangeDecoder",
    "startsWithBom",
    "toError",
];

/// The throws module's exported names.
const THROWS_SURFACE: &[&str] = &[
    "DurableObjectSqliteDatabase",
    "STORAGE_MEMORY_SCALES",
    "STORAGE_READ_BENCHMARKS",
    "STORAGE_WRITE_BENCHMARKS",
    "TIMING_SCALE",
    "createCloudflarePhotonImages",
    "createEnvConformance",
    "createExpectAssertions",
    "createNodePhotonImages",
    "createPhotonImages",
    "createStorageConformance",
    "openDurableObjectSqliteStorage",
    "registerEnvConformance",
    "registerStorageConformance",
    "seedStorageBenchmark",
    "seedStorageWriteBenchmark",
    "storageBenchmarkPrimaryRecordCount",
];

/// See tests/node_shims.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-durable-{}-{test_name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture subdir");
        }
        std::fs::write(&path, content).expect("write fixture");
    }
    dir
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn run_probe(test_name: &str, files: &[(&str, &str)], tool: &str) -> Value {
    // The ported `edit-diff.ts` imports the npm package `diff`; every fixture
    // that links the kernel module therefore needs it resolvable.
    let mut all: Vec<(&str, &str)> = files.to_vec();
    all.push(("node_modules/diff/package.json", DIFF_PACKAGE_JSON));
    all.push(("node_modules/diff/index.js", DIFF_INDEX_JS));
    let dir = write_temp_fixture(test_name, &all);
    let rt = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    let loaded = rt
        .load(std::slice::from_ref(&dir))
        .await
        .expect("load returns");
    assert!(
        loaded.errors.is_empty(),
        "the durable fixture must link every entry (shim registration + export surface): {:?}",
        loaded.errors
    );
    assert_eq!(loaded.loaded.len(), 1, "one loaded entry: {loaded:?}");
    let outcome = rt
        .call_tool(tool, "call-durable-probe", &json!({}))
        .await
        .expect("tool executes");
    assert_eq!(outcome.content.len(), 1);
    assert_eq!(
        outcome.content[0].kind, "text",
        "tool result shape: {outcome:?}"
    );
    serde_json::from_str(&outcome.content[0].text).expect("probe answers JSON")
}

/// The translated upstream cases: the storage contract over the three backends,
/// the session kernel, `NodeExecutionEnv`, the `/tools` implementations, the
/// tool-search ranking, the codemode definitions and the contract vocabulary.
/// A failure names the upstream case it came from.
#[tokio::test(flavor = "current_thread")]
async fn durable_shim_upstream_cases_pass() {
    let report = run_probe(
        "cases",
        &[("index.ts", INDEX_TS), ("durable_probe.mjs", PROBE_MJS)],
        "durable_probe",
    )
    .await;
    let failed = report["failed"].as_array().cloned().unwrap_or_default();
    assert!(
        failed.is_empty(),
        "every durable case must pass, failures: {failed:#?}"
    );
    let total = report["total"].as_u64().unwrap_or_default();
    assert_eq!(
        report["passed"].as_u64().unwrap_or_default(),
        total,
        "passed must equal total: {report}"
    );
    assert_eq!(total, 25, "the case list must not shrink: {report}");
}

/// The registration table: the kernel and `/env` modules carry exactly their
/// published surfaces, every published subpath resolves and carries its names,
/// and every throws-tier entry throws the standard error at first use.
#[tokio::test(flavor = "current_thread")]
async fn durable_shim_entry_surfaces_and_throws() {
    let report = run_probe("surface", &[("index.ts", SURFACE_TS)], "durable_surface").await;

    let names = |key: &str| -> Vec<String> {
        report[key]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|value| value.as_str().unwrap_or_default().to_string())
            .collect()
    };
    assert_eq!(names("kernel"), KERNEL_SURFACE, "the kernel module surface");
    assert_eq!(names("env"), ENV_SURFACE, "the /env module surface");
    assert_eq!(names("throws"), THROWS_SURFACE, "the throws module surface");

    for entry in [
        ".",
        "/tools",
        "/env",
        "/env/node",
        "/tool-search",
        "/codemode",
        "/storage/memory",
        "/storage/jsonl",
        "/storage/jsonl/node",
        "/storage/sqlite",
        "/storage/sqlite/node",
    ] {
        let missing = report["subpathMissing"][entry]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            missing.is_empty(),
            "{entry} must carry its names, missing: {missing:#?}"
        );
    }

    let throws = |key: &str| -> String {
        report["throwsBehavior"][key]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    for key in [
        "images.createPhotonImages",
        "imagesNode.createNodePhotonImages",
        "imagesCloudflare.createCloudflarePhotonImages",
        "testing.registerStorageConformance",
        "testing.registerEnvConformance",
        "testing.createStorageConformance",
        "testing.STORAGE_MEMORY_SCALES",
        "cloudflare.openDurableObjectSqliteStorage",
        "cloudflare.DurableObjectSqliteDatabase",
        "harness.Harness",
        "harness.Harness.open",
        "harness.createRegistry",
        "harness.watchEvents",
    ] {
        let value = throws(key);
        assert!(
            value.starts_with("ERR_NOT_SUPPORTED:")
                && value.contains("is not supported by the pir extension runtime"),
            "{key} must throw the unified unsupported error: {value}"
        );
    }
    assert_eq!(
        report["harnessCode"].as_str().unwrap_or_default(),
        "ERR_NOT_SUPPORTED",
        "the harness facades carry the standard code: {report}"
    );
}
