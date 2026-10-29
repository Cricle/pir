import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { withRetry } = require('../src/index.js');

for (const maxRetries of [0, 1, 2, 3, 5]) {
  const delays = [];
  let calls = 0;
  const failure = new Error('always unavailable');
  failure.status = 500;
  let thrown;
  try {
    await withRetry(async () => {
      calls += 1;
      throw failure;
    }, {
      maxRetries,
      baseDelayMs: 1,
      sleep: async (ms) => delays.push(ms),
    });
  } catch (error) {
    thrown = error;
  }
  assert.equal(thrown, failure);
  assert.equal(calls, maxRetries + 1, `maxRetries=${maxRetries} means ${maxRetries + 1} total attempts`);
  assert.equal(delays.length, maxRetries, `maxRetries=${maxRetries} means ${maxRetries} sleeps`);
  assert.equal(thrown.attempts, maxRetries + 1);
}
console.log('ok - maxRetries counts retries after the initial attempt');
