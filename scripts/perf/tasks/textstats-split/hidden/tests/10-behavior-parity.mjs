import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const old = require('../lib/textstats.js');
const index = require('../lib/textstats/index.js');

const corpus = 'Alpha beta, beta gamma! Don’t stop: alpha-beta 42.';
for (const name of ['tokenize', 'splitSentences', 'wordFrequency', 'topWords', 'summarize']) {
  const args = name === 'wordFrequency' ? [corpus] : [corpus, 3];
  assert.deepEqual(index[name](...args), old[name](...args), `${name} must behave identically`);
}

const rows = [['a', 1], ['bbb', 22]];
const options = { headers: ['name', 'n'], align: ['left', 'right'] };
assert.equal(index.formatTable(rows, options), old.formatTable(rows, options));
assert.equal(
  index.formatSummary(old.summarize(corpus)),
  old.formatSummary(old.summarize(corpus)),
);

assert.throws(() => index.tokenize(null), TypeError);
assert.throws(() => index.summarize('x', { top: -1 }), RangeError);
assert.throws(() => index.formatTable([['x']], { align: 'bad' }), RangeError);
console.log('ok - old and new entry points are behaviorally identical');
