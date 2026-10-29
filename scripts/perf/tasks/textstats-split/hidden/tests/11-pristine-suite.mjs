import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const root = resolve(HERE, '..');

assert.ok(
  existsSync(join(root, 'lib', 'textstats', 'index.js')),
  'the new lib/textstats/index.js entry point must exist',
);

const testFile = join(HERE, '_pristine', 'stats.test.cjs');
const result = spawnSync(process.execPath, ['--test', testFile], {
  cwd: root,
  encoding: 'utf8',
  timeout: 20_000,
  env: { ...process.env, NODE_OPTIONS: '' },
});
assert.equal(result.status, 0, `the original compatibility suite must still pass:\n${result.stdout}\n${result.stderr}`);
console.log('ok - the original visible suite still passes on the compatibility shim');
