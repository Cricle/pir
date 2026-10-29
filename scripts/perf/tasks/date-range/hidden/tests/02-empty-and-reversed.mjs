import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { enumerateDates } = require('../src/date-range.js');

assert.deepEqual(enumerateDates('2024-02-28', '2024-02-28'), []);
assert.deepEqual(enumerateDates('2024-02-29', '2024-02-29'), []);
assert.deepEqual(enumerateDates('2024-01-01', '2024-01-01'), []);

assert.throws(() => enumerateDates('2024-03-01', '2024-02-01'), RangeError);
assert.throws(() => enumerateDates('2025-01-01', '2024-12-31'), RangeError);
assert.throws(() => enumerateDates('2024-03-01', '2024-03-01T00:00:00Z'), RangeError);
console.log('ok - empty ranges are empty and reversed ranges throw');
