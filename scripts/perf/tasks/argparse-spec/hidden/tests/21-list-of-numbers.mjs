import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { withOptions } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

// A list is a list of raw strings even when the values look numeric.
const spec = withOptions({ nums: { type: 'list', short: 'N' } });
const parsed = parseArgs(['--nums=-5', '-N1.5', '--nums=2'], spec);
assert.deepEqual(parsed.options.nums, ['-5', '1.5', '2']);
console.log('ok - list values stay strings');
