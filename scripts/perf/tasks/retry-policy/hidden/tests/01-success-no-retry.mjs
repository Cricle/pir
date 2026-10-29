import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { withRetry } = require('../src/index.js');

const delays = [];
const retries = [];
let calls = 0;
const result = await withRetry(async (attempt) => {
  calls += 1;
  assert.equal(attempt, 1);
  return 'value';
}, {
  sleep: async (ms) => delays.push(ms),
  onRetry: (event) => retries.push(event),
});

assert.equal(result, 'value');
assert.equal(calls, 1);
assert.deepEqual(delays, []);
assert.deepEqual(retries, []);
console.log('ok - success on the first attempt is not retried');
