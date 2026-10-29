import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { withRetry } = require('../src/index.js');

const delays = [];
let calls = 0;
await assert.rejects(withRetry(async () => {
  calls += 1;
  const error = new Error('unavailable');
  error.status = 502;
  throw error;
}, {
  maxRetries: 3,
  baseDelayMs: 100,
  maxDelayMs: 150,
  sleep: async (ms) => delays.push(ms),
}));

assert.equal(calls, 4);
assert.deepEqual(delays, [100, 150, 150], 'each delay is capped independently');
console.log('ok - maxDelayMs caps every delay');
