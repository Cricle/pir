#!/usr/bin/env node
/**
 * Hidden-test verdict for the argparse-spec task.
 * Copies the work dir, drops hidden tests into __hidden__/, runs each test in
 * its own process and prints {task, passed, failed, total, score, notes}.
 */
import { cpSync, mkdtempSync, readdirSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const TASK = 'argparse-spec';
const HERE = dirname(fileURLToPath(import.meta.url));
const TEST_DIR = join(HERE, 'tests');
const workdir = resolve(process.argv[2] ?? '.');
const timeoutMs = Number(process.env.VERDICT_TIMEOUT_MS ?? 30_000);
const SKIP = new Set(['.git', 'node_modules', '__hidden__', '__verdict__', 'target', 'target-alt']);

function fail(message) {
  console.error(`verdict: ${message}`);
  process.exit(1);
}

if (!statSync(workdir).isDirectory()) fail(`not a directory: ${workdir}`);

const tmp = mkdtempSync(join(tmpdir(), `pir-${TASK}-`));
const copy = join(tmp, 'work');
const notes = [];
let passed = 0;
let failed = 0;

try {
  cpSync(workdir, copy, {
    recursive: true,
    filter: (src) => !SKIP.has(basename(src)),
  });

  const hiddenDest = join(copy, '__hidden__');
  cpSync(TEST_DIR, hiddenDest, { recursive: true });

  const tests = readdirSync(hiddenDest)
    .filter((name) => /^\d.*\.mjs$/.test(name))
    .sort();

  if (tests.length === 0) fail(`no hidden tests found in ${TEST_DIR}`);

  for (const name of tests) {
    const result = spawnSync(process.execPath, [join('__hidden__', name)], {
      cwd: copy,
      encoding: 'utf8',
      timeout: timeoutMs,
      env: { ...process.env, NODE_OPTIONS: '', VERDICT_WORKDIR: copy },
    });

    if (result.status === 0) {
      passed += 1;
      continue;
    }

    failed += 1;
    const text = `${result.stderr ?? ''}\n${result.stdout ?? ''}`.trim();
    const lines = text.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
    const detail = result.error?.message
      ?? lines.find((line) => line.includes('Error'))
      ?? lines[lines.length - 1]
      ?? `exit ${result.status}`;
    notes.push(`${name}: ${detail.slice(0, 240)}`);
  }
} finally {
  rmSync(tmp, { recursive: true, force: true });
}

const total = passed + failed;
const score = total === 0 ? 0 : passed / total;
console.log(JSON.stringify({ task: TASK, passed, failed, total, score, notes }, null, 2));
