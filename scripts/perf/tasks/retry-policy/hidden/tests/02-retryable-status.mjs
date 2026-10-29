import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { withRetry } = require('../src/index.js');

const delays = [];
let calls = 0;
const result = await withRetry(async (attempt) => {
  calls += 1;
  if (calls === 1) {
    const error = new Error('rate limited');
    error.status = 429;
    throw error;
  }
  if (calls === 2) {
    const error = new Error('unavailable');
    error.status = 503;
    throw error;
  }
  assert.equal(attempt, 3);
  return 'recovered';
}, {
  maxRetries: 3,
  baseDelayMs: 100,
  sleep: async (ms) => delays.push(ms),
});

assert.equal(result, 'recovered');
assert.equal(calls, 3);
assert.deepEqual(delays, [100, 200]);
console.log('ok - 429/503 are retried with the documented backoff');
