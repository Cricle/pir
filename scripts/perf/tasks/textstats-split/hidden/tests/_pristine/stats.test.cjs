'use strict';

// Byte-for-byte behavior copy of the original visible suite, with only the
// require path adjusted for its hidden location.
const test = require('node:test');
const assert = require('node:assert/strict');

const stats = require('../../lib/textstats.js');

test('tokenize normalizes case and keeps internal punctuation', () => {
  assert.deepEqual(stats.tokenize("Don’t stop—the 3D-printed café!"), [
    "don't", 'stop', 'the', '3d-printed', 'café',
  ]);
  assert.deepEqual(stats.tokenize('Hello HELLO', { lower: false }), ['Hello', 'HELLO']);
  assert.deepEqual(stats.tokenize('a bb ccc', { minLength: 2 }), ['bb', 'ccc']);
});

test('splitSentences drops separators and empty pieces', () => {
  assert.deepEqual(stats.splitSentences('One. Two!  Three?'), ['One', 'Two', 'Three']);
  assert.deepEqual(stats.splitSentences('   '), []);
});

test('wordFrequency sorts by count then token', () => {
  assert.deepEqual(stats.wordFrequency(['b', 'a', 'b', 'c', 'a']), [
    { token: 'a', count: 2 },
    { token: 'b', count: 2 },
    { token: 'c', count: 1 },
  ]);
});

test('topWords limits the list', () => {
  assert.deepEqual(stats.topWords('b a b c a', 2), [
    { token: 'a', count: 2 },
    { token: 'b', count: 2 },
  ]);
});

test('summarize reports counts and stopword-free top words', () => {
  const summary = stats.summarize('The cat sat. The cat ran.');
  assert.equal(summary.characters, 25);
  assert.equal(summary.words, 6);
  assert.equal(summary.uniqueWords, 4);
  assert.equal(summary.sentences, 2);
  assert.deepEqual(summary.topWords, [
    { token: 'cat', count: 2 },
    { token: 'ran', count: 1 },
    { token: 'sat', count: 1 },
  ]);
});

test('formatTable aligns and renders headers', () => {
  assert.equal(
    stats.formatTable([['a', 1], ['bbb', 22]], { headers: ['name', 'n'], align: ['left', 'right'] }),
    'name |  n\na    |  1\nbbb  | 22',
  );
});

test('formatSummary renders the summary shape', () => {
  assert.equal(
    stats.formatSummary({
      characters: 8,
      words: 2,
      uniqueWords: 2,
      sentences: 1,
      topWords: [{ token: 'hi', count: 2 }],
    }),
    'Characters: 8\nWords: 2 (2 unique)\nSentences: 1\nTop words:\n  1. hi (2)',
  );
});
