import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

const parsed = parseArgs(['--name', 'Alice', 'file.txt', '--verbose'], baseSpec);
assert.equal(parsed.options.name, 'Alice');
assert.equal(parsed.options.verbose, true);
assert.equal(parsed.options.quiet, true);
assert.equal(parsed.options.count, 1);
assert.deepEqual(parsed.options.tag, []);
assert.equal(parsed.options.output, undefined);
assert.deepEqual(parsed.positionals, ['file.txt']);
console.log('ok - long options with separate values and defaults');
