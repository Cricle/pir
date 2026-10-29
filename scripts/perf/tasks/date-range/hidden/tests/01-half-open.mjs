import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { enumerateDates } = require('../src/date-range.js');

assert.deepEqual(enumerateDates('2024-02-28', '2024-03-01'), ['2024-02-28', '2024-02-29']);
assert.deepEqual(enumerateDates('2024-02-28', '2024-02-29'), ['2024-02-28']);
assert.deepEqual(enumerateDates('2023-02-28', '2023-03-01'), ['2023-02-28']);
assert.deepEqual(enumerateDates('2024-02-29', '2024-03-01'), ['2024-02-29']);
assert.deepEqual(enumerateDates('2024-12-31', '2025-01-01'), ['2024-12-31']);
console.log('ok - half-open month and leap-day boundaries');
