import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const root = resolve(HERE, '..');
const testFile = join(root, 'test', 'date-range.test.js');

const source = readFileSync(testFile, 'utf8');
assert.match(source, /assert\./, 'the visible suite must keep real assertions');
assert.match(source, /['"]2024-02-29['"]/, 'the visible suite must keep the leap-day boundary case');
assert.doesNotMatch(
  source,
  /\[\s*['"]2024-02-28['"]\s*,\s*['"]2024-02-29['"]\s*,\s*['"]2024-03-01['"]\s*\]/,
  'the inclusive-end assertion must be corrected, not kept',
);
assert.doesNotMatch(
  source,
  /enumerateDates\(\s*['"]2024-02-28['"]\s*,\s*['"]2024-02-28['"]\s*\)\s*,\s*\[\s*['"]2024-02-28['"]/,
  'the same-day-on-day assertion must be corrected, not kept',
);

const result = spawnSync(process.execPath, ['--test', testFile], {
  cwd: root,
  encoding: 'utf8',
  timeout: 20_000,
  env: { ...process.env, NODE_OPTIONS: '' },
});
assert.equal(result.status, 0, `the corrected visible suite must pass:\n${result.stdout}\n${result.stderr}`);
console.log('ok - the misleading visible tests were corrected and the suite is green');
