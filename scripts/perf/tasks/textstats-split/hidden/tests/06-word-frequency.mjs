import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { wordFrequency } = require('../lib/textstats/index.js');

assert.deepEqual(wordFrequency(['b', 'a', 'b', 'c', 'a']), [
  { token: 'a', count: 2 },
  { token: 'b', count: 2 },
  { token: 'c', count: 1 },
]);
assert.deepEqual(wordFrequency('B a b A'), [
  { token: 'a', count: 2 },
  { token: 'b', count: 2 },
]);
assert.deepEqual(wordFrequency([]), []);
assert.throws(() => wordFrequency(['a', 1]), TypeError);
assert.throws(() => wordFrequency(42), TypeError);
console.log('ok - wordFrequency accepts strings/arrays and sorts deterministically');
