import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { splitArgs } = require('../src/parse-args.js');

assert.deepEqual(splitArgs('  a\tb   c  '), ['a', 'b', 'c']);
assert.deepEqual(splitArgs(''), []);
assert.deepEqual(splitArgs('   \t  '), []);
assert.deepEqual(splitArgs('""'), ['']);
assert.deepEqual(splitArgs('a "" b'), ['a', '', 'b']);
assert.deepEqual(splitArgs('one'), ['one']);
console.log('ok - splitArgs handles whitespace and empty quoted tokens');
