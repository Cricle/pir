import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export const ROOT = process.cwd();
export const CLI = join(ROOT, 'logq.js');

export function run(args, options = {}) {
  return spawnSync(process.execPath, [CLI, ...args], {
    cwd: ROOT,
    encoding: 'utf8',
    input: options.input,
    timeout: options.timeout ?? 20_000,
    env: { ...process.env, NODE_OPTIONS: '' },
  });
}

export function jsonLines(stdout) {
  return stdout
    .split(/\r?\n/)
    .filter((line) => line.length > 0)
    .map((line) => JSON.parse(line));
}

export function tempFile(content, name = 'input.log') {
  const dir = mkdtempSync(join(tmpdir(), 'logq-test-'));
  const path = join(dir, name);
  writeFileSync(path, content, 'utf8');
  return {
    dir,
    path,
    cleanup() {
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

export function recordsFile(records, name = 'input.log') {
  const content = records.map((record) => JSON.stringify(record)).join('\n') + '\n';
  return tempFile(content, name);
}
