import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { tokenize, splitSentences } = require('../lib/textstats/index.js');

assert.deepEqual(tokenize("Don’t stop—the 3D-printed café!"), [
  "don't", 'stop', 'the', '3d-printed', 'café',
]);
assert.deepEqual(tokenize('Hello HELLO', { lower: false }), ['Hello', 'HELLO']);
assert.deepEqual(tokenize('a bb ccc', { minLength: 2 }), ['bb', 'ccc']);
assert.deepEqual(tokenize('...!!!...'), []);
assert.deepEqual(tokenize(''), []);
assert.throws(() => tokenize(42), TypeError);
assert.throws(() => tokenize('x', { minLength: -1 }), RangeError);
assert.throws(() => tokenize('x', { minLength: 1.5 }), RangeError);

assert.deepEqual(splitSentences('One. Two!  Three?'), ['One', 'Two', 'Three']);
assert.deepEqual(splitSentences('   '), []);
assert.deepEqual(splitSentences('No terminator'), ['No terminator']);
assert.throws(() => splitSentences(null), TypeError);
console.log('ok - tokenize/splitSentences behavior and validation');
