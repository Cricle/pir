import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { expectArgError } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { splitArgs } = require('../src/parse-args.js');

assert.deepEqual(splitArgs('a\\ b'), ['a b']);
assert.deepEqual(splitArgs('a\\\\b'), ['a\\b']);
assert.deepEqual(splitArgs('a\\tb'), ['atb']);
assert.deepEqual(splitArgs('\\"quoted\\"'), ['"quoted"']);

expectArgError(() => splitArgs('abc\\'), 'DANGLING_ESCAPE');
expectArgError(() => splitArgs('"abc\\'), 'DANGLING_ESCAPE');
console.log('ok - backslash escapes outside quotes and dangling escapes throw');
