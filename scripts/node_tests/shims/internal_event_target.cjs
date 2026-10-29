// Shim for `internal/event_target` — Node's EventTarget implementation
// internals. The vendored subset (test-events-once.js) reads `kEvents` to
// count raw listeners on an AbortSignal. QuickJS's pir runtime keeps its own
// listener storage, so this symbol cannot be wired to the real count; the
// stub makes the file load and those four size assertions report the real
// verdict.
'use strict';

const kEvents = Symbol('kEvents');

module.exports = {
  kEvents,
  kTarget: Symbol('kTarget'),
  kHandlers: Symbol('kHandlers'),
  isEventTarget(obj) {
    return obj != null && typeof obj === 'object' && typeof obj.addEventListener === 'function';
  },
};
