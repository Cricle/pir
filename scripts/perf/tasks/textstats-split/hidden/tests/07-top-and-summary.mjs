import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { topWords, summarize } = require('../lib/textstats/index.js');

assert.deepEqual(topWords('b a b c a', 2), [
  { token: 'a', count: 2 },
  { token: 'b', count: 2 },
]);
assert.deepEqual(topWords('x', 0), []);
const many = Array.from({ length: 12 }, (_, index) => `w${index}`).join(' ');
assert.equal(topWords(many).length, 10);
assert.equal(topWords(many, 3).length, 3);
assert.throws(() => topWords('x', -1), RangeError);
assert.throws(() => topWords('x', 1.5), RangeError);

const summary = summarize('The cat sat. The cat ran.');
assert.deepEqual(summary, {
  characters: 25,
  words: 6,
  uniqueWords: 4,
  sentences: 2,
  topWords: [
    { token: 'cat', count: 2 },
    { token: 'ran', count: 1 },
    { token: 'sat', count: 1 },
  ],
});

const custom = summarize('The cat sat. The cat ran.', { top: 1, stopwords: new Set(['cat', 'the']) });
assert.deepEqual(custom.topWords, [{ token: 'ran', count: 1 }]);
assert.deepEqual(summarize(''), {
  characters: 0,
  words: 0,
  uniqueWords: 0,
  sentences: 0,
  topWords: [],
});
assert.throws(() => summarize('x', { top: -1 }), RangeError);
assert.throws(() => summarize('x', { stopwords: ['the'] }), TypeError);
console.log('ok - topWords and summarize edge cases');
