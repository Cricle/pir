import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { splitArgs } = require('../src/parse-args.js');

assert.deepEqual(splitArgs(`a 'b c' d`), ['a', 'b c', 'd']);
assert.deepEqual(splitArgs(`'a\\b'`), ['a\\b']);
assert.deepEqual(splitArgs('ab"c d"'), ['abc d']);
assert.deepEqual(splitArgs('"a \\"b\\" c"'), ['a "b" c']);
assert.deepEqual(splitArgs('"a\\\\b"'), ['a\\b']);
assert.deepEqual(splitArgs('"a\\nb"'), ['a\\nb']);
assert.deepEqual(splitArgs(`'it''s'`), ['its']);
console.log('ok - single/double quoting and inside-quote escaping');
