import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { formatSummary } = require('../lib/textstats/index.js');

assert.equal(
  formatSummary({
    characters: 8,
    words: 2,
    uniqueWords: 2,
    sentences: 1,
    topWords: [{ token: 'hi', count: 2 }],
  }),
  'Characters: 8\nWords: 2 (2 unique)\nSentences: 1\nTop words:\n  1. hi (2)',
);

assert.equal(
  formatSummary({ characters: 0, words: 0, uniqueWords: 0, sentences: 0, topWords: [] }),
  'Characters: 0\nWords: 0 (0 unique)\nSentences: 0\nTop words:\n  (none)',
);
assert.throws(() => formatSummary(null), TypeError);
assert.throws(() => formatSummary('x'), TypeError);
console.log('ok - formatSummary exact rendering');
