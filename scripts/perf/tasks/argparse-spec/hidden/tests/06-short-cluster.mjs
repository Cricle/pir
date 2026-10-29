import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

const parsed = parseArgs(['-vq'], baseSpec);
assert.equal(parsed.options.verbose, true);
assert.equal(parsed.options.quiet, true);

const three = parseArgs(['-vq', '--no-quiet'], baseSpec);
assert.equal(three.options.quiet, false);

const positional = parseArgs(['-v', 'file'], baseSpec);
assert.deepEqual(positional.positionals, ['file']);
console.log('ok - clustered boolean shorts');
