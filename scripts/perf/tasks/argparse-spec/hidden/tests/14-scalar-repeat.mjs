import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

const parsed = parseArgs(['--name', 'a', '--name=b', '-n', 'c'], baseSpec);
assert.equal(parsed.options.name, 'c');
assert.equal(parseArgs(['--count', '2', '-c3'], baseSpec).options.count, 3);
assert.equal(parseArgs(['--verbose', '--no-verbose', '--verbose'], baseSpec).options.verbose, true);
assert.equal(parseArgs(['--verbose', '--no-verbose'], baseSpec).options.verbose, false);
console.log('ok - repeated scalar options use the last occurrence');
