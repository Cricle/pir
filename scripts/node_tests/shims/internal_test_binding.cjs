// Shim for `internal/test/binding` — Node's test-only accessor to internal
// C++ bindings. Only what the vendored subset actually touches is provided;
// everything else returns an empty object so the requiring file loads and
// the assertions around the touched surface decide pass/fail.
'use strict';

const { EventEmitter } = require('node:events');

// test-util-inspect.js: `internalBinding('js_stream').JSStream` — inspected
// for its constructor name in an inspect-formatting assertion.
class JSStream extends EventEmitter {}

const bindings = {
  js_stream: { JSStream },
  // test-util-inspect.js: `internalBinding('util').previewEntries` — used by
  // util.inspect preview formatting for WeakMap/WeakSet/typed-array internals.
  // A conservative stub: no preview entries, not a keyed preview.
  util: { previewEntries: () => [[], false] },
};

module.exports = {
  internalBinding(name) {
    return bindings[name] || {};
  },
};
