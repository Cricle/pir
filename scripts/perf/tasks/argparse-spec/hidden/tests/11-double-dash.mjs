import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

const parsed = parseArgs(['--verbose', '--', '--name', 'x', '-v', '-'], baseSpec);
assert.equal(parsed.options.verbose, true);
assert.equal(parsed.options.name, undefined);
assert.deepEqual(parsed.positionals, ['--name', 'x', '-v', '-']);

const terminator = parseArgs(['--', '--'], baseSpec);
assert.deepEqual(terminator.positionals, ['--']);

const before = parseArgs(['a', '--verbose', 'b'], baseSpec);
assert.deepEqual(before.positionals, ['a', 'b']);
assert.equal(before.options.verbose, true);
console.log('ok - -- terminates option parsing and everything after is positional');
