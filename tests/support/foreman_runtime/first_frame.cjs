// For the local, trusted fixtures in this directory; vm is not a security sandbox.
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const [mode, file] = process.argv.slice(2);
assert(mode === 'syntax' || mode === 'first-frame', 'expected syntax or first-frame');
const html = fs.readFileSync(file, 'utf8');
const scripts = [...html.matchAll(/<script>([\s\S]*?)<\/script>/gi)];
assert.equal(scripts.length, 1, 'expected exactly one inline script');
const script = new vm.Script(scripts[0][1], { filename: file });
if (mode === 'syntax') {
  console.log('SYNTAX_OK');
} else {
  const pending = [];
  const sandbox = vm.createContext({
    requestAnimationFrame(callback) { pending.push(callback); },
  });
  const options = { timeout: 1000 };
  script.runInContext(sandbox, options);
  assert.equal(pending.length, 1, 'loading the page must queue the first frame');
  sandbox.firstFrame = pending.shift();
  // Run the callback under the same timeout, before any simulated user input.
  vm.runInContext('firstFrame(0)', sandbox, options);
  assert.equal(pending.length, 1, 'the first frame must queue a successor');
  console.log('FIRST_FRAME_OK');
}
