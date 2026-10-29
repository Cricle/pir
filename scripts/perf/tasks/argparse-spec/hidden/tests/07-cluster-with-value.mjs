import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec, expectArgError } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

const attached = parseArgs(['-vc5'], baseSpec);
assert.equal(attached.options.verbose, true);
assert.equal(attached.options.count, 5);

// In -vtn, the value-taking t consumes the rest of the token ('n').
const separate = parseArgs(['-vtn', 'Bob'], baseSpec);
assert.equal(separate.options.verbose, true);
assert.deepEqual(separate.options.tag, ['n']);
assert.deepEqual(separate.positionals, ['Bob']);

assert.deepEqual(parseArgs(['-vtn', 'x'], baseSpec).options.tag, ['n']);

expectArgError(() => parseArgs(['-vcx'], baseSpec), 'INVALID_NUMBER', { option: 'c', value: 'x' });
assert.equal(parseArgs(['-vc-5'], baseSpec).options.count, -5);
console.log('ok - a value-taking short consumes the rest of its cluster');
