import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { withRetry } = require('../src/index.js');

const delays = [];
let calls = 0;
await assert.rejects(withRetry(async () => {
  calls += 1;
  const error = new Error('unavailable');
  error.status = 503;
  throw error;
}, {
  maxRetries: 4,
  baseDelayMs: 50,
  maxDelayMs: 10_000,
  sleep: async (ms) => delays.push(ms),
}));

assert.equal(calls, 5);
assert.deepEqual(delays, [50, 100, 200, 400], 'retry n waits base * 2 ** (n - 1)');
console.log('ok - exponential backoff starts at baseDelayMs');
