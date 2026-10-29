import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const root = resolve(HERE, '..');
const limits = {
  'tokenize.js': 160,
  'stats.js': 160,
  'format.js': 160,
  'index.js': 60,
};

for (const [file, limit] of Object.entries(limits)) {
  const source = readFileSync(join(root, 'lib', 'textstats', file), 'utf8');
  const lines = source.split(/\r?\n/).length;
  assert.ok(lines > 0, `${file} must not be empty`);
  assert.ok(lines <= limit, `${file} must be <= ${limit} lines, got ${lines}`);
}
console.log('ok - every split module respects its line budget');
