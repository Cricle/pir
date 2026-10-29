//! P0b wave: the node builtin name-surface lock test. Every shimmed
//! builtin's `Object.keys` (named exports) must cover the real
//! Node v24 value-export list (superset allowed for documented extras).

use std::path::Path;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

const NODE_SURFACE_TS: &str = r##"
import * as NodeModuleRegistry from "node:module";

const NODE_SURFACE: Record<string, string[]> = Object.assign(Object.create(null), {
  "fs": ["Dir", "Dirent", "FileReadStream", "FileWriteStream", "ReadStream", "Stats", "Utf8Stream", "WriteStream", "_toUnixTimestamp", "access", "accessSync", "appendFile", "appendFileSync", "chmod", "chmodSync", "chown", "chownSync", "close", "closeSync", "constants", "copyFile", "copyFileSync", "cp", "cpSync", "createReadStream", "createWriteStream", "exists", "existsSync", "fchmod", "fchmodSync", "fchown", "fchownSync", "fdatasync", "fdatasyncSync", "fstat", "fstatSync", "fsync", "fsyncSync", "ftruncate", "ftruncateSync", "futimes", "futimesSync", "glob", "globSync", "lchmod", "lchmodSync", "lchown", "lchownSync", "link", "linkSync", "lstat", "lstatSync", "lutimes", "lutimesSync", "mkdir", "mkdirSync", "mkdtemp", "mkdtempDisposableSync", "mkdtempSync", "open", "openAsBlob", "openSync", "opendir", "opendirSync", "promises", "read", "readFile", "readFileSync", "readSync", "readdir", "readdirSync", "readlink", "readlinkSync", "readv", "readvSync", "realpath", "realpathSync", "rename", "renameSync", "rm", "rmSync", "rmdir", "rmdirSync", "stat", "statSync", "statfs", "statfsSync", "symlink", "symlinkSync", "truncate", "truncateSync", "unlink", "unlinkSync", "unwatchFile", "utimes", "utimesSync", "watch", "watchFile", "write", "writeFile", "writeFileSync", "writeSync", "writev", "writevSync"],
  "fs/promises": ["access", "appendFile", "chmod", "chown", "constants", "copyFile", "cp", "glob", "lchmod", "lchown", "link", "lstat", "lutimes", "mkdir", "mkdtemp", "mkdtempDisposable", "open", "opendir", "readFile", "readdir", "readlink", "realpath", "rename", "rm", "rmdir", "stat", "statfs", "symlink", "truncate", "unlink", "utimes", "watch", "writeFile"],
  "path": ["_makeLong", "basename", "delimiter", "dirname", "extname", "format", "isAbsolute", "join", "matchesGlob", "normalize", "parse", "posix", "relative", "resolve", "sep", "toNamespacedPath", "win32"],
  "path/posix": ["_makeLong", "basename", "delimiter", "dirname", "extname", "format", "isAbsolute", "join", "matchesGlob", "normalize", "parse", "posix", "relative", "resolve", "sep", "toNamespacedPath", "win32"],
  "os": ["EOL", "arch", "availableParallelism", "constants", "cpus", "devNull", "endianness", "freemem", "getPriority", "homedir", "hostname", "loadavg", "machine", "networkInterfaces", "platform", "release", "setPriority", "tmpdir", "totalmem", "type", "uptime", "userInfo", "version"],
  "crypto": ["Certificate", "Cipheriv", "Decipheriv", "DiffieHellman", "DiffieHellmanGroup", "ECDH", "Hash", "Hmac", "KeyObject", "Sign", "Verify", "X509Certificate", "argon2", "argon2Sync", "checkPrime", "checkPrimeSync", "constants", "createCipheriv", "createDecipheriv", "createDiffieHellman", "createDiffieHellmanGroup", "createECDH", "createHash", "createHmac", "createPrivateKey", "createPublicKey", "createSecretKey", "createSign", "createVerify", "decapsulate", "diffieHellman", "encapsulate", "generateKey", "generateKeyPair", "generateKeyPairSync", "generateKeySync", "generatePrime", "generatePrimeSync", "getCipherInfo", "getCiphers", "getCurves", "getDiffieHellman", "getFips", "getHashes", "getRandomValues", "hash", "hkdf", "hkdfSync", "pbkdf2", "pbkdf2Sync", "privateDecrypt", "privateEncrypt", "publicDecrypt", "publicEncrypt", "randomBytes", "randomFill", "randomFillSync", "randomInt", "randomUUID", "randomUUIDv7", "scrypt", "scryptSync", "secureHeapUsed", "setEngine", "setFips", "sign", "subtle", "timingSafeEqual", "verify", "webcrypto"],
  "buffer": ["Blob", "Buffer", "File", "INSPECT_MAX_BYTES", "SlowBuffer", "atob", "btoa", "constants", "isAscii", "isUtf8", "kMaxLength", "kStringMaxLength", "resolveObjectURL", "transcode"],
  "child_process": ["ChildProcess", "_forkChild", "exec", "execFile", "execFileSync", "execSync", "fork", "spawn", "spawnSync"],
  "stream": ["Duplex", "PassThrough", "Readable", "Stream", "Transform", "Writable", "_isArrayBufferView", "_isUint8Array", "_uint8ArrayToBuffer", "addAbortSignal", "compose", "destroy", "duplexPair", "finished", "getDefaultHighWaterMark", "isDestroyed", "isDisturbed", "isErrored", "isReadable", "isWritable", "pipeline", "promises", "setDefaultHighWaterMark"],
  "stream/promises": ["finished", "pipeline"],
  "zlib": ["BrotliCompress", "BrotliDecompress", "Deflate", "DeflateRaw", "Gunzip", "Gzip", "Inflate", "InflateRaw", "Unzip", "ZstdCompress", "ZstdDecompress", "brotliCompress", "brotliCompressSync", "brotliDecompress", "brotliDecompressSync", "codes", "constants", "crc32", "createBrotliCompress", "createBrotliDecompress", "createDeflate", "createDeflateRaw", "createGunzip", "createGzip", "createInflate", "createInflateRaw", "createUnzip", "createZstdCompress", "createZstdDecompress", "deflate", "deflateRaw", "deflateRawSync", "deflateSync", "gunzip", "gunzipSync", "gzip", "gzipSync", "inflate", "inflateRaw", "inflateRawSync", "inflateSync", "unzip", "unzipSync", "zstdCompress", "zstdCompressSync", "zstdDecompress", "zstdDecompressSync"],
  "module": ["Module", "SourceMap", "_cache", "_debug", "_extensions", "_findPath", "_initPaths", "_load", "_nodeModulePaths", "_pathCache", "_preloadModules", "_resolveFilename", "_resolveLookupPaths", "builtinModules", "constants", "createRequire", "enableCompileCache", "findPackageJSON", "findSourceMap", "flushCompileCache", "getCompileCacheDir", "getSourceMapsSupport", "globalPaths", "isBuiltin", "register", "registerHooks", "runMain", "setSourceMapsSupport", "stripTypeScriptTypes", "syncBuiltinESMExports"],
  "events": ["EventEmitter", "EventEmitterAsyncResource", "addAbortListener", "captureRejectionSymbol", "captureRejections", "defaultMaxListeners", "errorMonitor", "getEventListeners", "getMaxListeners", "init", "listenerCount", "on", "once", "setMaxListeners", "usingDomains"],
  "util": ["MIMEParams", "MIMEType", "TextDecoder", "TextEncoder", "_errnoException", "_exceptionWithHostPort", "_extend", "aborted", "callbackify", "convertProcessSignalToExitCode", "debug", "debuglog", "deprecate", "diff", "format", "formatWithOptions", "getCallSites", "getSystemErrorMap", "getSystemErrorMessage", "getSystemErrorName", "inherits", "inspect", "isArray", "isDeepStrictEqual", "parseArgs", "parseEnv", "promisify", "setTraceSigInt", "stripVTControlCharacters", "styleText", "toUSVString", "transferableAbortController", "transferableAbortSignal", "types"],
  "util/types": ["isAnyArrayBuffer", "isArgumentsObject", "isArrayBuffer", "isArrayBufferView", "isAsyncFunction", "isBigInt64Array", "isBigIntObject", "isBigUint64Array", "isBooleanObject", "isBoxedPrimitive", "isCryptoKey", "isDataView", "isDate", "isExternal", "isFloat16Array", "isFloat32Array", "isFloat64Array", "isGeneratorFunction", "isGeneratorObject", "isInt16Array", "isInt32Array", "isInt8Array", "isKeyObject", "isMap", "isMapIterator", "isModuleNamespaceObject", "isNativeError", "isNumberObject", "isPromise", "isProxy", "isRegExp", "isSet", "isSetIterator", "isSharedArrayBuffer", "isStringObject", "isSymbolObject", "isTypedArray", "isUint16Array", "isUint32Array", "isUint8Array", "isUint8ClampedArray", "isWeakMap", "isWeakSet"],
  "url": ["URL", "URLPattern", "URLSearchParams", "Url", "domainToASCII", "domainToUnicode", "fileURLToPath", "fileURLToPathBuffer", "format", "parse", "pathToFileURL", "resolve", "resolveObject", "urlToHttpOptions"],
  "assert": ["Assert", "AssertionError", "CallTracker", "deepEqual", "deepStrictEqual", "doesNotMatch", "doesNotReject", "doesNotThrow", "equal", "fail", "ifError", "match", "notDeepEqual", "notDeepStrictEqual", "notEqual", "notStrictEqual", "ok", "partialDeepStrictEqual", "rejects", "strict", "strictEqual", "throws"],
  "assert/strict": ["Assert", "AssertionError", "CallTracker", "deepEqual", "deepStrictEqual", "doesNotMatch", "doesNotReject", "doesNotThrow", "equal", "fail", "ifError", "match", "notDeepEqual", "notDeepStrictEqual", "notEqual", "notStrictEqual", "ok", "partialDeepStrictEqual", "rejects", "strict", "strictEqual", "throws"],
  "querystring": ["decode", "encode", "escape", "parse", "stringify", "unescape", "unescapeBuffer"],
  "string_decoder": ["StringDecoder"],
  "timers": ["clearImmediate", "clearInterval", "clearTimeout", "promises", "setImmediate", "setInterval", "setTimeout"],
  "timers/promises": ["scheduler", "setImmediate", "setInterval", "setTimeout"],
  "perf_hooks": ["Performance", "PerformanceEntry", "PerformanceMark", "PerformanceMeasure", "PerformanceObserver", "PerformanceObserverEntryList", "PerformanceResourceTiming", "constants", "createHistogram", "eventLoopUtilization", "monitorEventLoopDelay", "performance", "timerify"],
  "net": ["BlockList", "BoundSocket", "Server", "Socket", "SocketAddress", "Stream", "_createServerHandle", "_normalizeArgs", "connect", "createConnection", "createServer", "getDefaultAutoSelectFamily", "getDefaultAutoSelectFamilyAttemptTimeout", "isIP", "isIPv4", "isIPv6", "setDefaultAutoSelectFamily", "setDefaultAutoSelectFamilyAttemptTimeout"],
  "http": ["Agent", "ClientRequest", "CloseEvent", "IncomingMessage", "METHODS", "MessageEvent", "OutgoingMessage", "STATUS_CODES", "Server", "ServerResponse", "WebSocket", "_connectionListener", "createServer", "get", "globalAgent", "maxHeaderSize", "request", "setGlobalProxyFromEnv", "setMaxIdleHTTPParsers", "validateHeaderName", "validateHeaderValue"],
  "https": ["Agent", "Server", "createServer", "get", "globalAgent", "request"],
  "tls": ["CLIENT_RENEG_LIMIT", "CLIENT_RENEG_WINDOW", "DEFAULT_CIPHERS", "DEFAULT_ECDH_CURVE", "DEFAULT_MAX_VERSION", "DEFAULT_MIN_VERSION", "SecureContext", "Server", "TLSSocket", "checkServerIdentity", "connect", "convertALPNProtocols", "createSecureContext", "createServer", "getCACertificates", "getCertificateCompressionAlgorithms", "getCiphers", "rootCertificates", "setDefaultCACertificates"],
  "dns": ["ADDRCONFIG", "ADDRGETNETWORKPARAMS", "ALL", "BADFAMILY", "BADFLAGS", "BADHINTS", "BADNAME", "BADQUERY", "BADRESP", "BADSTR", "CANCELLED", "CONNREFUSED", "DESTRUCTION", "EOF", "FILE", "FORMERR", "LOADIPHLPAPI", "NODATA", "NOMEM", "NONAME", "NOTFOUND", "NOTIMP", "NOTINITIALIZED", "REFUSED", "Resolver", "SERVFAIL", "TIMEOUT", "V4MAPPED", "getDefaultResultOrder", "getServers", "lookup", "lookupService", "promises", "resolve", "resolve4", "resolve6", "resolveAny", "resolveCaa", "resolveCname", "resolveMx", "resolveNaptr", "resolveNs", "resolvePtr", "resolveSoa", "resolveSrv", "resolveTlsa", "resolveTxt", "reverse", "setDefaultResultOrder", "setServers"],
  "dns/promises": ["ADDRGETNETWORKPARAMS", "BADFAMILY", "BADFLAGS", "BADHINTS", "BADNAME", "BADQUERY", "BADRESP", "BADSTR", "CANCELLED", "CONNREFUSED", "DESTRUCTION", "EOF", "FILE", "FORMERR", "LOADIPHLPAPI", "NODATA", "NOMEM", "NONAME", "NOTFOUND", "NOTIMP", "NOTINITIALIZED", "REFUSED", "Resolver", "SERVFAIL", "TIMEOUT", "getDefaultResultOrder", "getServers", "lookup", "lookupService", "resolve", "resolve4", "resolve6", "resolveAny", "resolveCaa", "resolveCname", "resolveMx", "resolveNaptr", "resolveNs", "resolvePtr", "resolveSoa", "resolveSrv", "resolveTlsa", "resolveTxt", "reverse", "setDefaultResultOrder", "setServers"],
  "async_hooks": ["AsyncLocalStorage", "AsyncResource", "asyncWrapProviders", "createHook", "executionAsyncId", "executionAsyncResource", "triggerAsyncId"],
  "worker_threads": ["BroadcastChannel", "MessageChannel", "MessagePort", "SHARE_ENV", "Worker", "getEnvironmentData", "isInternalThread", "isMainThread", "isMarkedAsUntransferable", "locks", "markAsUncloneable", "markAsUntransferable", "moveMessagePortToContext", "parentPort", "postMessageToThread", "receiveMessageOnPort", "resourceLimits", "setEnvironmentData", "threadId", "threadName", "workerData"],
  "tty": ["ReadStream", "WriteStream", "isatty"],
  "readline": ["Interface", "clearLine", "clearScreenDown", "createInterface", "cursorTo", "emitKeypressEvents", "moveCursor", "promises"],
  "v8": ["DefaultDeserializer", "DefaultSerializer", "Deserializer", "GCProfiler", "Serializer", "cachedDataVersionTag", "deserialize", "getCppHeapStatistics", "getHeapCodeStatistics", "getHeapSnapshot", "getHeapSpaceStatistics", "getHeapStatistics", "isStringOneByteRepresentation", "promiseHooks", "queryObjects", "serialize", "setFlagsFromString", "setHeapSnapshotNearHeapLimit", "startCpuProfile", "startupSnapshot", "stopCoverage", "takeCoverage", "writeHeapSnapshot"],
  "vm": ["Script", "compileFunction", "constants", "createContext", "createScript", "isContext", "measureMemory", "runInContext", "runInNewContext", "runInThisContext"],
  "constants": ["COPYFILE_EXCL", "COPYFILE_FICLONE", "COPYFILE_FICLONE_FORCE", "DH_CHECK_P_NOT_PRIME", "DH_CHECK_P_NOT_SAFE_PRIME", "DH_NOT_SUITABLE_GENERATOR", "DH_UNABLE_TO_CHECK_GENERATOR", "E2BIG", "EACCES", "EADDRINUSE", "EADDRNOTAVAIL", "EAFNOSUPPORT", "EAGAIN", "EALREADY", "EBADF", "EBADMSG", "EBUSY", "ECANCELED", "ECHILD", "ECONNABORTED", "ECONNREFUSED", "ECONNRESET", "EDEADLK", "EDESTADDRREQ", "EDOM", "EEXIST", "EFAULT", "EFBIG", "EHOSTUNREACH", "EIDRM", "EILSEQ", "EINPROGRESS", "EINTR", "EINVAL", "EIO", "EISCONN", "EISDIR", "ELOOP", "EMFILE", "EMLINK", "EMSGSIZE", "ENAMETOOLONG", "ENETDOWN", "ENETRESET", "ENETUNREACH", "ENFILE", "ENGINE_METHOD_ALL", "ENGINE_METHOD_CIPHERS", "ENGINE_METHOD_DH", "ENGINE_METHOD_DIGESTS", "ENGINE_METHOD_DSA", "ENGINE_METHOD_EC", "ENGINE_METHOD_NONE", "ENGINE_METHOD_PKEY_ASN1_METHS", "ENGINE_METHOD_PKEY_METHS", "ENGINE_METHOD_RAND", "ENGINE_METHOD_RSA", "ENOBUFS", "ENODATA", "ENODEV", "ENOENT", "ENOEXEC", "ENOLCK", "ENOLINK", "ENOMEM", "ENOMSG", "ENOPROTOOPT", "ENOSPC", "ENOSR", "ENOSTR", "ENOSYS", "ENOTCONN", "ENOTDIR", "ENOTEMPTY", "ENOTSOCK", "ENOTSUP", "ENOTTY", "ENXIO", "EOPNOTSUPP", "EOVERFLOW", "EPERM", "EPIPE", "EPROTO", "EPROTONOSUPPORT", "EPROTOTYPE", "ERANGE", "EROFS", "ESPIPE", "ESRCH", "ETIME", "ETIMEDOUT", "ETXTBSY", "EWOULDBLOCK", "EXDEV", "F_OK", "OPENSSL_VERSION_NUMBER", "O_APPEND", "O_CREAT", "O_EXCL", "O_RDONLY", "O_RDWR", "O_TRUNC", "O_WRONLY", "POINT_CONVERSION_COMPRESSED", "POINT_CONVERSION_HYBRID", "POINT_CONVERSION_UNCOMPRESSED", "PRIORITY_ABOVE_NORMAL", "PRIORITY_BELOW_NORMAL", "PRIORITY_HIGH", "PRIORITY_HIGHEST", "PRIORITY_LOW", "PRIORITY_NORMAL", "RSA_NO_PADDING", "RSA_PKCS1_OAEP_PADDING", "RSA_PKCS1_PADDING", "RSA_PKCS1_PSS_PADDING", "RSA_PSS_SALTLEN_AUTO", "RSA_PSS_SALTLEN_DIGEST", "RSA_PSS_SALTLEN_MAX_SIGN", "RSA_X931_PADDING", "R_OK", "SIGABRT", "SIGBREAK", "SIGFPE", "SIGHUP", "SIGILL", "SIGINT", "SIGKILL", "SIGQUIT", "SIGSEGV", "SIGTERM", "SIGWINCH", "SSL_OP_ALL", "SSL_OP_ALLOW_NO_DHE_KEX", "SSL_OP_ALLOW_UNSAFE_LEGACY_RENEGOTIATION", "SSL_OP_CIPHER_SERVER_PREFERENCE", "SSL_OP_CISCO_ANYCONNECT", "SSL_OP_COOKIE_EXCHANGE", "SSL_OP_CRYPTOPRO_TLSEXT_BUG", "SSL_OP_DONT_INSERT_EMPTY_FRAGMENTS", "SSL_OP_LEGACY_SERVER_CONNECT", "SSL_OP_NO_COMPRESSION", "SSL_OP_NO_ENCRYPT_THEN_MAC", "SSL_OP_NO_QUERY_MTU", "SSL_OP_NO_RENEGOTIATION", "SSL_OP_NO_SESSION_RESUMPTION_ON_RENEGOTIATION", "SSL_OP_NO_SSLv2", "SSL_OP_NO_SSLv3", "SSL_OP_NO_TICKET", "SSL_OP_NO_TLSv1", "SSL_OP_NO_TLSv1_1", "SSL_OP_NO_TLSv1_2", "SSL_OP_NO_TLSv1_3", "SSL_OP_PRIORITIZE_CHACHA", "SSL_OP_TLS_ROLLBACK_BUG", "S_IFCHR", "S_IFDIR", "S_IFIFO", "S_IFLNK", "S_IFMT", "S_IFREG", "S_IRUSR", "S_IWUSR", "TLS1_1_VERSION", "TLS1_2_VERSION", "TLS1_3_VERSION", "TLS1_VERSION", "UV_DIRENT_BLOCK", "UV_DIRENT_CHAR", "UV_DIRENT_DIR", "UV_DIRENT_FIFO", "UV_DIRENT_FILE", "UV_DIRENT_LINK", "UV_DIRENT_SOCKET", "UV_DIRENT_UNKNOWN", "UV_FS_COPYFILE_EXCL", "UV_FS_COPYFILE_FICLONE", "UV_FS_COPYFILE_FICLONE_FORCE", "UV_FS_O_FILEMAP", "UV_FS_SYMLINK_DIR", "UV_FS_SYMLINK_JUNCTION", "WSAEACCES", "WSAEADDRINUSE", "WSAEADDRNOTAVAIL", "WSAEAFNOSUPPORT", "WSAEALREADY", "WSAEBADF", "WSAECANCELLED", "WSAECONNABORTED", "WSAECONNREFUSED", "WSAECONNRESET", "WSAEDESTADDRREQ", "WSAEDISCON", "WSAEDQUOT", "WSAEFAULT", "WSAEHOSTDOWN", "WSAEHOSTUNREACH", "WSAEINPROGRESS", "WSAEINTR", "WSAEINVAL", "WSAEINVALIDPROCTABLE", "WSAEINVALIDPROVIDER", "WSAEISCONN", "WSAELOOP", "WSAEMFILE", "WSAEMSGSIZE", "WSAENAMETOOLONG", "WSAENETDOWN", "WSAENETRESET", "WSAENETUNREACH", "WSAENOBUFS", "WSAENOMORE", "WSAENOPROTOOPT", "WSAENOTCONN", "WSAENOTEMPTY", "WSAENOTSOCK", "WSAEOPNOTSUPP", "WSAEPFNOSUPPORT", "WSAEPROCLIM", "WSAEPROTONOSUPPORT", "WSAEPROTOTYPE", "WSAEPROVIDERFAILEDINIT", "WSAEREFUSED", "WSAEREMOTE", "WSAESHUTDOWN", "WSAESOCKTNOSUPPORT", "WSAESTALE", "WSAETIMEDOUT", "WSAETOOMANYREFS", "WSAEUSERS", "WSAEWOULDBLOCK", "WSANOTINITIALISED", "WSASERVICE_NOT_FOUND", "WSASYSCALLFAILURE", "WSASYSNOTREADY", "WSATYPE_NOT_FOUND", "WSAVERNOTSUPPORTED", "WSA_E_CANCELLED", "WSA_E_NO_MORE", "W_OK", "X_OK", "defaultCoreCipherList"],
  "punycode": ["decode", "encode", "toASCII", "toUnicode", "ucs2", "version"],
  "console": ["log", "info", "debug", "warn", "error", "dir", "time", "timeEnd", "timeLog", "trace", "assert", "clear", "count", "countReset", "group", "groupEnd", "table", "dirxml", "groupCollapsed", "Console", "profile", "profileEnd", "timeStamp", "context", "createTask"],
  "sys": ["_errnoException", "_exceptionWithHostPort", "_extend", "callbackify", "convertProcessSignalToExitCode", "debug", "debuglog", "deprecate", "format", "styleText", "formatWithOptions", "getCallSites", "getSystemErrorMap", "getSystemErrorName", "getSystemErrorMessage", "inherits", "inspect", "isArray", "isDeepStrictEqual", "promisify", "stripVTControlCharacters", "toUSVString", "transferableAbortSignal", "transferableAbortController", "aborted", "parseEnv", "parseArgs", "TextDecoder", "TextEncoder", "MIMEType", "MIMEParams", "diff", "setTraceSigInt", "types"],
  "diagnostics_channel": ["channel", "hasSubscribers", "subscribe", "tracingChannel", "unsubscribe", "Channel"],
  "stream/web": ["ReadableStream", "ReadableStreamTee", "ReadableStreamDefaultReader", "ReadableStreamBYOBReader", "ReadableStreamBYOBRequest", "ReadableByteStreamController", "ReadableStreamDefaultController", "TransformStream", "TransformStreamDefaultController", "WritableStream", "WritableStreamDefaultWriter", "WritableStreamDefaultController", "ByteLengthQueuingStrategy", "CountQueuingStrategy", "TextEncoderStream", "TextDecoderStream", "CompressionStream", "DecompressionStream"],
  "stream/consumers": ["arrayBuffer", "blob", "buffer", "bytes", "text", "json"],
  "readline/promises": ["Interface", "Readline", "createInterface"],
  "inspector": ["Session", "console", "Network", "NetworkResources", "DOMStorage", "open", "close", "url", "waitForDebugger"],
  "inspector/promises": ["Session", "console", "Network", "NetworkResources", "DOMStorage", "open", "close", "url", "waitForDebugger"],
  "http2": ["connect", "createServer", "createSecureServer", "getDefaultSettings", "getPackedSettings", "getUnpackedSettings", "performServerHandshake", "Http2ServerRequest", "Http2ServerResponse", "constants"],
  "cluster": ["Worker", "setupPrimary", "setupMaster", "fork", "disconnect", "_events", "_eventsCount", "_maxListeners", "isWorker", "isMaster", "isPrimary", "workers", "settings", "SCHED_NONE", "SCHED_RR", "schedulingPolicy"],
  "dgram": ["_createSocketHandle", "createSocket", "Socket"],
  "domain": ["Domain", "createDomain", "create", "_stack", "active"],
  "repl": ["start", "writer", "REPLServer", "Recoverable", "isValidSyntax"],
  "trace_events": ["createTracing", "getEnabledCategories"],
  "wasi": ["WASI"],
  "test": ["expectFailure", "skip", "todo", "only", "after", "afterEach", "before", "beforeEach", "describe", "getTestContext", "it", "run", "suite", "test", "mock", "snapshot", "assert"],
  "test/reporters": ["dot", "junit", "spec", "tap", "lcov"],
  "sea": ["isSea", "getAsset", "getRawAsset", "getAssetAsBlob", "getAssetKeys"],
  "_http_agent": ["Agent", "globalAgent"],
  "_http_client": ["ClientRequest"],
  "_http_common": ["_checkInvalidHeaderChar", "_checkIsHttpToken", "freeParser", "HTTPParser", "isLenient", "calculateLenientFlags", "prepareError", "chunkExpression", "continueExpression", "CRLF", "methods", "parsers", "kIncomingMessage", "kSkipPendingData"],
  "_http_incoming": ["IncomingMessage", "readStart", "readStop"],
  "_http_outgoing": ["parseUniqueHeadersOption", "validateHeaderName", "validateHeaderValue", "OutgoingMessage", "kHighWaterMark", "kUniqueHeaders"],
  "_http_server": ["Server", "ServerResponse", "setupConnectionsTracking", "storeHTTPOptions", "_connectionListener", "httpServerPreClose", "STATUS_CODES", "kServerResponse", "kConnectionsCheckingInterval"],
  "_stream_duplex": ["fromWeb", "toWeb", "from"],
  "_stream_passthrough": [],
  "_stream_readable": ["ReadableState", "_fromList", "from", "fromWeb", "toWeb", "wrap"],
  "_stream_transform": [],
  "_stream_wrap": [],
  "_stream_writable": ["WritableState", "fromWeb", "toWeb"],
  "_tls_common": ["SecureContext", "createSecureContext", "translatePeerCertificate"],
  "_tls_wrap": ["TLSSocket", "Server", "createServer", "connect"],
});

// throws-tier modules: the entry point that must throw the unified error
// when invoked (construct for classes, call for functions).
const THROWS_SURFACE: Array<[string, string, boolean]> = [
  ["http2", "connect", false],
  ["http2", "createServer", false],
  ["cluster", "fork", false],
  ["cluster", "Worker", true],
  ["dgram", "createSocket", false],
  ["dgram", "Socket", true],
  ["repl", "start", false],
  ["repl", "REPLServer", true],
  ["trace_events", "createTracing", false],
  ["domain", "create", false],
  ["domain", "Domain", true],
  ["wasi", "WASI", true],
  ["test", "test", false],
  ["test", "describe", false],
  ["test/reporters", "tap", true],
  ["sea", "getAsset", false],
  ["sea", "getRawAsset", false],
  ["_stream_duplex", "fromWeb", false],
  ["_stream_readable", "fromWeb", false],
  ["_stream_writable", "toWeb", false],
];

// Names the real win32 Node exposes as own keys with value `undefined`
// (`Object.keys` contains them, the value does not). They are still locked in:
// the binding has to exist.
const UNDEFINED_OK: Record<string, string[]> = Object.assign(Object.create(null), {
  "fs": ["lchmod", "lchmodSync"],
});

export default (pi: any) => {
  pi.on("session_start", async () => {
    const missing: string[] = [];
    const moduleNames: string[] = [];
    for (const moduleName of Object.keys(NODE_SURFACE)) {
      moduleNames.push(moduleName);
      const names = NODE_SURFACE[moduleName];
      let ns: any;
      try {
        ns = await import("@node/" + moduleName);
      } catch (error) {
        missing.push(moduleName + " :: <import failed: " + String(error) + ">");
        continue;
      }
      for (const name of names) {
        // Platform-conditional value exports: on win32 the real Node publishes
        // `fs.lchmod`/`fs.lchmodSync` as own keys whose value is `undefined`
        // (libuv has no lchmod there), so the lock can only require the
        // *binding* to exist — requiring a value would be stricter than Node
        // itself. The official test-fs-chmod.js depends on exactly that
        // truthiness (`if (fs.lchmod)` skips its macOS-only branch).
        const undefinedOk = UNDEFINED_OK[moduleName];
        if (undefinedOk !== undefined && undefinedOk.indexOf(name) !== -1) {
          if (!(name in ns)) missing.push(moduleName + " :: " + name);
          continue;
        }
        if (ns[name] === undefined) {
          missing.push(moduleName + " :: " + name);
        }
      }
    }

    // throws-tier runtime assertion: the listed entries must throw the
    // unified message with a code when invoked.
    const throwsBad: string[] = [];
    for (const [moduleName, api, construct] of THROWS_SURFACE) {
      let ns: any;
      try {
        ns = await import("@node/" + moduleName);
      } catch (error) {
        throwsBad.push(moduleName + " :: <import failed: " + String(error) + ">");
        continue;
      }
      const entry = ns[api];
      if (typeof entry !== "function") {
        throwsBad.push(moduleName + " :: " + api + " <not callable>");
        continue;
      }
      let threw: any = null;
      try {
        if (construct) {
          new entry();
        } else {
          entry();
        }
      } catch (error: any) {
        threw = error;
      }
      if (threw === null) {
        throwsBad.push(moduleName + " :: " + api + " <no throw>");
      } else if (
        String(threw.message || "").indexOf("is not supported by the pir extension runtime") === -1 ||
        !threw.code
      ) {
        throwsBad.push(moduleName + " :: " + api + " <wrong error: " + String(threw.message) + ">");
      }
    }

    // Honest-value assertions for the near-real residuals.
    const valueBad: string[] = [];
    try {
      const sea: any = await import("@node/sea");
      if (sea.isSea() !== false) valueBad.push("sea.isSea");
      if (sea.getAssetKeys().next().done !== true) valueBad.push("sea.getAssetKeys");
      const dc: any = await import("@node/diagnostics_channel");
      let received: any = null;
      dc.subscribe("pir:test", (message: any) => { received = message; });
      dc.channel("pir:test").publish({ v: 1 });
      if (!received || received.v !== 1) valueBad.push("diagnostics_channel.publish");
      if (dc.channel("pir:test").hasSubscribers !== true) valueBad.push("diagnostics_channel.hasSubscribers");
      const h2: any = await import("@node/http2");
      if (h2.getDefaultSettings().headerTableSize !== 4096) valueBad.push("http2.getDefaultSettings");
      if (h2.constants.HTTP2_HEADER_PATH !== ":path") valueBad.push("http2.constants");
      const cluster: any = await import("@node/cluster");
      if (cluster.isWorker !== false || cluster.isPrimary !== true) valueBad.push("cluster.state");
      const cons: any = await import("@node/console");
      if (typeof cons.Console !== "function" || typeof cons.log !== "function") valueBad.push("console.surface");
      const sysMod: any = await import("@node/sys");
      if (typeof sysMod.format !== "function" || typeof sysMod.inspect !== "function") valueBad.push("sys.alias");
      const rlPromises: any = await import("@node/readline/promises");
      const answer: string = await rlPromises.createInterface({}).question("?");
      if (typeof answer !== "string") valueBad.push("readline/promises.question");
      const consumers: any = await import("@node/stream/consumers");
      const consumedText: string = await consumers.text("s");
      if (consumedText !== "s") valueBad.push("stream/consumers.text");
      const streamWeb: any = await import("@node/stream/web");
      if (typeof streamWeb.ReadableStream !== "function") valueBad.push("stream/web.ReadableStream");
      const te = new streamWeb.TextEncoderStream();
      if (!te.readable || !te.writable) valueBad.push("stream/web.TextEncoderStream");

      // Web-compat globals pad.
      if (typeof globalThis.queueMicrotask !== "function") valueBad.push("globalThis.queueMicrotask");
      if (typeof globalThis.structuredClone !== "function") valueBad.push("globalThis.structuredClone");
      const cloned: any = globalThis.structuredClone({ a: [1, 2], m: new Map([["k", "v"]]), d: new Date(1234), s: new Set([7]), r: /x/g });
      if (cloned.a[1] !== 2 || cloned.m.get("k") !== "v" || cloned.d.getTime() !== 1234 || !cloned.s.has(7) || cloned.r.global !== true) valueBad.push("structuredClone.roundtrip");
      let clonedCircularThrew = false;
      const circular: any = { self: undefined };
      circular.self = circular;
      try { globalThis.structuredClone(circular); } catch { clonedCircularThrew = true; }
      if (!clonedCircularThrew) valueBad.push("structuredClone.circular");
      if (typeof globalThis.atob !== "function" || globalThis.atob(globalThis.btoa("pir")) !== "pir") valueBad.push("globalThis.atob/btoa");
      if (typeof globalThis.performance.now !== "function") valueBad.push("globalThis.performance");
      if (typeof globalThis.Blob !== "function" || typeof globalThis.File !== "function") valueBad.push("globalThis.Blob/File");
      const blobText: string = await new globalThis.Blob(["web blob"]).text();
      if (blobText !== "web blob") valueBad.push("globalThis.Blob.text");
      if (typeof globalThis.URL !== "function" || typeof globalThis.URLSearchParams !== "function") valueBad.push("globalThis.URL");
      if (typeof globalThis.fetch !== "function") valueBad.push("globalThis.fetch");
    } catch (error: any) {
      valueBad.push("<exception: " + String(error && error.stack ? error.stack : error) + ">");
    }

    globalThis.__nodeSurface = {
      moduleCount: moduleNames.length,
      missing,
      throwsBad,
      valueBad,
      registry: globalThis.__pirBuiltinModuleNames,
    };
  });
};
"##;

#[tokio::test(flavor = "current_thread")]
async fn node_surface_name_lock() {
    let dir = std::env::temp_dir().join("pir-node-surface-lock");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let rt = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
    rt.seed_module("pir-ext:node-surface.ts", NODE_SURFACE_TS)
        .expect("seed node-surface fixture");
    let loaded = rt.seeded_load("pir-ext:node-surface.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture must load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors while locking the node surface: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__nodeSurface)")
        .await
        .expect("read node surface results");

    assert_eq!(v["missing"], json!([]), "missing node surface names: {v}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// node:vm minimal-real behavior (same-realm sandbox approximation): the
// sandbox semantics real consumers rely on (pi-subagents scripted-workflow)
// plus the host-isolation approximation assertions.
// ---------------------------------------------------------------------------

const VM_BEHAVIOR_TS: &str = r##"
import vm from "node:vm";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const bad: string[] = [];
    const eq = (what: string, got: any, want: any) => {
      if (got !== want) bad.push(what + " (got " + String(got) + ", want " + String(want) + ")");
    };

    // sandbox plumbing: writes land on the sandbox, reads resolve through it,
    // function-valued members stay callable, completion values are returned.
    const sb: any = { x: 1, emit(v: any) { return v * 2; } };
    const ctx = vm.createContext(sb);
    eq("isContext.sandbox", vm.isContext(sb), true);
    eq("isContext.plain", vm.isContext({}), false);
    eq("completion", vm.runInNewContext("1+1"), 2);
    eq("read", vm.runInContext("x + 1", ctx), 2);
    eq("memberFn", vm.runInContext("emit(21)", ctx), 42);
    vm.runInContext("globalThis.newG = 5", ctx);
    eq("globalThisWrite", sb.newG, 5);
    eq("seededJson", vm.runInContext("JSON.stringify({a:1})", ctx), "{\"a\":1}");
    eq("seededMath", vm.runInContext("Math.max(2,3)", ctx), 3);
    // top-level `return` is a Program grammar error in real Node too.
    let topReturn = "accepted";
    try { vm.runInNewContext("return 1"); } catch (e: any) { topReturn = e.constructor.name; }
    eq("topReturn", topReturn, "SyntaxError");

    // isolation approximation: host globals are not reachable by bare name.
    eq("isolated.process", vm.runInContext("typeof process", ctx), "undefined");
    eq("isolated.Buffer", vm.runInContext("typeof Buffer", ctx), "undefined");
    eq("globalThisIsView", vm.runInContext("typeof globalThis.process", ctx), "undefined");

    // Script/createScript: eager syntax validation + context runs.
    eq("script", new vm.Script("x * 10").runInContext(ctx), 10);
    eq("createScript", vm.createScript("2+3").runInThisContext(), 5);
    let scriptSyntax = "accepted";
    try { new vm.Script("function ("); } catch (e: any) { scriptSyntax = e.constructor.name; }
    eq("scriptSyntaxError", scriptSyntax, "SyntaxError");

    // pi-subagents scripted-workflow shape: sandbox with a function member +
    // an async-IIFE script whose promise is the run result; the realm-shared
    // intrinsics keep Object.prototype/Promise.prototype identity.
    const sb2: any = { runs: { n: 2 }, args: { a: 40 }, console };
    const ctx2 = vm.createContext(sb2, { codeGeneration: { strings: false, wasm: false } });
    eq(
      "protoIdentity",
      vm.runInContext("Object.prototype", ctx2) === Object.prototype &&
        vm.runInContext("(async () => {})().constructor.prototype", ctx2) === Promise.prototype,
      true,
    );
    const compiled = new vm.Script("(async () => {\n return args.a + runs.n;\n})()", { filename: "workflow-script.js" });
    eq("awaitedWorkflow", await compiled.runInContext(ctx2), 42);

    // in-run declarations and errors surface with their own identities.
    eq("funcDeclInRun", vm.runInNewContext("function h(){ return 2 } h()"), 2);
    eq("varInRun", vm.runInNewContext("var vv = 8; vv + 1"), 9);

    // invalid context argument.
    let badCtx = "";
    try { vm.runInContext("1", {}); } catch (e: any) { badCtx = String(e.code || ""); }
    eq("invalidContextCode", badCtx, "ERR_INVALID_ARG_TYPE");

    globalThis.__vmBehavior = bad;
  });
};
"##;

#[tokio::test(flavor = "current_thread")]
async fn vm_sandbox_semantics() {
    let dir = std::env::temp_dir().join("pir-vm-behavior");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let rt = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
    rt.seed_module("pir-ext:vm-behavior.ts", VM_BEHAVIOR_TS)
        .expect("seed vm-behavior fixture");
    let loaded = rt.seeded_load("pir-ext:vm-behavior.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture must load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in vm behavior test: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__vmBehavior)")
        .await
        .expect("read vm behavior results");
    assert_eq!(v, json!([]), "vm behavior failures: {v}");
    let _ = std::fs::remove_dir_all(&dir);
}
