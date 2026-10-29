import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const root = resolve(HERE, '..');

function localRequires(file) {
  const source = readFileSync(join(root, 'lib', 'textstats', file), 'utf8');
  return [...source.matchAll(/require\(\s*['"]([^'"]+)['"]\s*\)/g)]
    .map((match) => match[1])
    .filter((specifier) => specifier.startsWith('.'))
    .map((specifier) => specifier.replace(/\.js$/, ''))
    .sort();
}

assert.deepEqual(localRequires('tokenize.js'), [], 'tokenize.js must be a leaf module');
assert.deepEqual(localRequires('format.js'), [], 'format.js must be a leaf module');
assert.deepEqual(localRequires('stats.js'), ['./tokenize'], 'stats.js may only depend on tokenize.js');
assert.deepEqual(
  localRequires('index.js'),
  ['./format', './stats', './tokenize'],
  'index.js must compose all three modules',
);
console.log('ok - split modules follow the required dependency direction');
