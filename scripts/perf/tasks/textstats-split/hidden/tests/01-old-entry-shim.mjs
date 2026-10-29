import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { EXPECTED_EXPORTS } from './_contract.mjs';

const require = createRequire(import.meta.url);
const HERE = dirname(fileURLToPath(import.meta.url));
const root = resolve(HERE, '..');
const source = readFileSync(join(root, 'lib', 'textstats.js'), 'utf8');

assert.ok(source.trim().length > 0, 'lib/textstats.js must not be empty');
const lineCount = source.split(/\r?\n/).length;
assert.ok(lineCount <= 10, `lib/textstats.js must stay a thin shim (got ${lineCount} lines)`);
assert.ok(!/\b(function|class)\b/.test(source), 'lib/textstats.js must not define functions or classes');
assert.match(
  source,
  /require\(\s*['"]\.\/textstats\/index(?:\.js)?['"]\s*\)/,
  'lib/textstats.js must re-export ./textstats/index.js',
);

const old = require('../lib/textstats.js');
assert.deepEqual(Object.keys(old).sort(), EXPECTED_EXPORTS);
assert.equal(typeof old.tokenize, 'function');
assert.equal(typeof old.formatSummary, 'function');
assert.ok(old.DEFAULT_STOPWORDS instanceof Set);
console.log('ok - the old entry point is a thin compatibility shim with the same exports');
