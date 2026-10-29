import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec, expectArgError } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

const parsed = parseArgs(['--verbose', '--no-quiet'], baseSpec);
assert.equal(parsed.options.verbose, true);
assert.equal(parsed.options.quiet, false);

const defaults = parseArgs([], baseSpec).options;
assert.equal(defaults.verbose, false);
assert.equal(defaults.quiet, true);

expectArgError(() => parseArgs(['--no-name'], baseSpec), 'UNKNOWN_OPTION', { option: 'no-name' });
console.log('ok - --no-<boolean> negates, and only booleans are negatable');
