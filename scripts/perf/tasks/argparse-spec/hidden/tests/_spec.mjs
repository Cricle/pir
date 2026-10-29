import assert from 'node:assert/strict';

export const baseSpec = {
  options: {
    verbose: { type: 'boolean', short: 'v' },
    quiet: { type: 'boolean', short: 'q', default: true },
    name: { type: 'string', short: 'n' },
    count: { type: 'number', short: 'c', default: 1 },
    tag: { type: 'list', short: 't' },
    output: { type: 'string', short: 'o' },
  },
};

export function withOptions(extra) {
  return { options: { ...baseSpec.options, ...extra } };
}

export function expectArgError(fn, code, properties = {}) {
  let error;
  try {
    fn();
  } catch (caught) {
    error = caught;
  }
  assert.ok(error, `expected ArgError ${code}, but nothing was thrown`);
  assert.equal(error.name, 'ArgError', `expected ArgError, got ${error.name}: ${error.message}`);
  assert.equal(error.code, code, `expected code ${code}, got ${error.code}: ${error.message}`);
  for (const [key, value] of Object.entries(properties)) {
    assert.equal(error[key], value, `expected .${key}=${JSON.stringify(value)}, got ${JSON.stringify(error[key])}`);
  }
  return error;
}
