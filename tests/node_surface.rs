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
        allow_process_exit: false,
        agent_dir: None,
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
  // `node:test` is a real in-process runner now and `node:test/reporters.tap`
  // is an async generator function (Node: `new tap()` is a TypeError, not a
  // "not supported by the pir extension runtime" error). Their names stay
  // locked by NODE_SURFACE above.
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
    // Unique per run: these tests remove-then-create the directory, so a shared
    // literal name would let a parallel or previous run delete a live fixture.
    let dir = crate::common::unique_temp_path("node-surface-lock");
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
    let dir = crate::common::unique_temp_path("vm-behavior");
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

// ---------------------------------------------------------------------------
// path/os parity corpus. Every expectation below was produced by running the
// same calls through Node v24.22.1 on win32 (and the posix namespace is
// independent of the host). The corpus also proves the shim actually reaches
// the typed native entry: the same table is replayed with `__pirPath` and
// `__pirNode` removed and must still match (the pure-JS fallback tier).
// ---------------------------------------------------------------------------

const PATH_OS_PARITY_TS: &str = r##"
import path from "node:path";
import os from "node:os";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const failures: string[] = [];
    let checked = 0;
    const P = (name: string, got: any, want: any) => {
      checked++;
      if (JSON.stringify(got) !== JSON.stringify(want)) {
        failures.push(name + ": got " + JSON.stringify(got) + ", want " + JSON.stringify(want));
      }
    };

    const WIN = path.win32;
    const POSIX = path.posix;
    const platformValue = (globalThis as any).__pirPlatform;

    const cwd = process.cwd();
    const slashCwd = cwd.replace(/\\/g, "/");
    const posixCwd = slashCwd.slice(slashCwd.indexOf("/"));
    const long60: string[] = [];
    for (let i = 0; i < 60; i++) long60.push("s" + i);

    const cases: Array<[string, () => any, any]> = [
      // --- posix join/normalize (trailing separators, empty segments, above root)
      ["posix.join0", () => POSIX.join(), "."],
      ["posix.joinEmpty", () => POSIX.join("", "", ""), "."],
      ["posix.joinSlash", () => POSIX.join("/", "/"), "/"],
      ["posix.joinMix", () => POSIX.join("a", "/b", "//c"), "a/b/c"],
      ["posix.joinDotDot", () => POSIX.join("a", "..", "..", "b"), "../b"],
      ["posix.joinTrail", () => POSIX.join("a/", "b/"), "a/b/"],
      ["posix.joinDot", () => POSIX.join("a/./b", ".", "c"), "a/b/c"],
      ["posix.joinAbs", () => POSIX.join("/a", "../../b"), "/b"],
      ["posix.joinLong", () => POSIX.join(...long60), long60.join("/")],
      ["posix.normEmpty", () => POSIX.normalize(""), "."],
      ["posix.normSlash", () => POSIX.normalize("//a//b"), "/a/b"],
      ["posix.normAbove", () => POSIX.normalize("/../../a"), "/a"],
      ["posix.normRelAbove", () => POSIX.normalize("../../a"), "../../a"],
      ["posix.normTrail", () => POSIX.normalize("a/b/"), "a/b/"],
      ["posix.normDot", () => POSIX.normalize("."), "."],
      ["posix.normDotSlash", () => POSIX.normalize("./"), "./"],
      ["posix.normOnlyDots", () => POSIX.normalize(".."), ".."],
      ["posix.normLong", () => POSIX.normalize(("/a/b/../c/./d//").repeat(30)), "/a/c/d".repeat(30) + "/"],
      ["posix.resolveEmpty", () => POSIX.resolve(), posixCwd],
      ["posix.resolveDot", () => POSIX.resolve("."), posixCwd],
      ["posix.resolveAbs", () => POSIX.resolve("/a/b", "../c", "d"), "/a/c/d"],
      ["posix.resolveRel", () => POSIX.resolve("a", "b", "..", "c"), posixCwd + "/a/c"],
      ["posix.resolveRoot", () => POSIX.resolve("/", "..", "..", "x"), "/x"],
      ["posix.relSame", () => POSIX.relative("/a/b", "/a/b"), ""],
      ["posix.relSibling", () => POSIX.relative("/a/b/c", "/a/b/d/e"), "../d/e"],
      ["posix.relUp", () => POSIX.relative("/a/b/c", "/a/d"), "../../d"],
      ["posix.relRoot", () => POSIX.relative("/", "/foo"), "foo"],
      ["posix.relToRoot", () => POSIX.relative("/foo/bar", "/"), "../.."],
      // --- win32 join (UNC, drive-relative, reserved names, mixed separators)
      ["win.join0", () => WIN.join(), "."],
      ["win.joinEmpty", () => WIN.join("", "", ""), "."],
      ["win.joinUNC", () => WIN.join("//server", "share"), "\\\\server\\share\\"],
      ["win.joinUNC2", () => WIN.join("\\\\server\\share", "x"), "\\\\server\\share\\x"],
      ["win.joinUNC3", () => WIN.join("\\\\server\\share\\", "..", "y"), "\\\\server\\share\\y"],
      ["win.joinDrive", () => WIN.join("C:\\a", "b", "..", "c"), "C:\\a\\c"],
      ["win.joinDriveRel", () => WIN.join("C:a", "b"), "C:a\\b"],
      ["win.joinMix", () => WIN.join("C:/a", "b\\c", "d"), "C:\\a\\b\\c\\d"],
      ["win.joinReserved", () => WIN.join("C:\\", "CON:", "file"), "C:\\\\CON:\\file"],
      ["win.joinLong", () => WIN.join(...long60), long60.join("\\")],
      // --- win32 normalize (drive roots, device/UNC roots, reserved names)
      ["win.normEmpty", () => WIN.normalize(""), "."],
      ["win.normRoot", () => WIN.normalize("\\"), "\\"],
      ["win.normSlash", () => WIN.normalize("/"), "\\"],
      ["win.normDrive", () => WIN.normalize("C:\\a\\..\\b\\\\c\\"), "C:\\b\\c\\"],
      ["win.normDriveRel", () => WIN.normalize("C:a\\..\\b"), "C:b"],
      ["win.normDriveOnly", () => WIN.normalize("C:"), "C:."],
      ["win.normDriveDot", () => WIN.normalize("C:."), "C:."],
      ["win.normCon", () => WIN.normalize("C:\\CON"), "C:\\CON"],
      ["win.normConColon", () => WIN.normalize("CON:"), ".\\CON:."],
      ["win.normConRel", () => WIN.normalize("CON:\\foo"), ".\\CON:foo"],
      ["win.normUNConly", () => WIN.normalize("\\\\server\\share"), "\\\\server\\share\\"],
      ["win.normUNCtrail", () => WIN.normalize("\\\\server\\share\\"), "\\\\server\\share\\"],
      ["win.normUNCtail", () => WIN.normalize("\\\\server\\share\\x\\..\\y"), "\\\\server\\share\\y"],
      ["win.normDevice", () => WIN.normalize("\\\\?\\C:\\foo\\..\\bar"), "\\\\?\\C:\\bar"],
      ["win.normDeviceDot", () => WIN.normalize("\\\\.\\PHYSICALDRIVE0"), "\\\\.\\PHYSICALDRIVE0"],
      ["win.normDeviceDev", () => WIN.normalize("\\\\?\\COM1:"), "\\\\?\\COM1:\\"],
      ["win.normAbove", () => WIN.normalize("C:\\..\\..\\a"), "C:\\a"],
      ["win.normRelAbove", () => WIN.normalize("..\\..\\a"), "..\\..\\a"],
      ["win.normTrail", () => WIN.normalize("a\\b\\"), "a\\b\\"],
      ["win.normColonTail", () => WIN.normalize("C:foo\\..\\bar"), "C:bar"],
      ["win.normCve", () => WIN.normalize("C:..\\..\\foo"), "C:..\\..\\foo"],
      ["win.normLong", () => WIN.normalize(("C:\\a\\b\\..\\c\\.\\d\\\\").repeat(30)), "C:\\a\\c\\d\\".repeat(30)],
      // --- win32 resolve/relative
      ["win.resolveEmpty", () => WIN.resolve(), cwd],
      ["win.resolveDot", () => WIN.resolve("."), cwd],
      ["win.resolveAbs", () => WIN.resolve("C:\\a", "b", "..", "c"), "C:\\a\\c"],
      ["win.resolveRel", () => WIN.resolve("a", "b", "..", "c"), cwd + "\\a\\c"],
      ["win.resolveUNC", () => WIN.resolve("\\\\server\\share", "x", "..", "y"), "\\\\server\\share\\y"],
      ["win.resolveDevice", () => WIN.resolve("\\\\?\\C:\\a", "b"), "\\\\?\\C:\\a\\b"],
      ["win.resolveRoot", () => WIN.resolve("C:\\", "..", "..", "x"), "C:\\x"],
      ["win.relSame", () => WIN.relative("C:\\a\\b", "C:\\a\\b"), ""],
      ["win.relSibling", () => WIN.relative("C:\\a\\b", "C:\\a\\c"), "..\\c"],
      ["win.relUp", () => WIN.relative("C:\\a\\b\\c", "C:\\a\\d"), "..\\..\\d"],
      ["win.relDrive", () => WIN.relative("C:\\a", "D:\\b"), "D:\\b"],
      ["win.relUNC", () => WIN.relative("\\\\s\\a\\b", "\\\\s\\a\\c"), "..\\c"],
      ["win.relTrail", () => WIN.relative("C:\\a\\b\\", "C:\\a\\b\\c"), "c"],
      ["win.relCase", () => WIN.relative("C:\\A\\B", "c:\\a\\b\\C"), "C"],
      // --- parse/format/basename/dirname/extname (drive + UNC edge cases)
      ["win.parseDrive", () => WIN.parse("C:\\a\\b.txt"), { root: "C:\\", dir: "C:\\a", base: "b.txt", ext: ".txt", name: "b" }],
      ["win.parseUNC", () => WIN.parse("\\\\server\\share\\dir\\file.txt"), { root: "\\\\server\\share\\", dir: "\\\\server\\share\\dir", base: "file.txt", ext: ".txt", name: "file" }],
      ["win.parseDot", () => WIN.parse("C:\\a\\.bashrc"), { root: "C:\\", dir: "C:\\a", base: ".bashrc", ext: "", name: ".bashrc" }],
      ["win.parseTrail", () => WIN.parse("C:\\a\\b\\"), { root: "C:\\", dir: "C:\\a", base: "b", ext: "", name: "b" }],
      ["win.formatDrive", () => WIN.format({ root: "C:\\", dir: "C:\\a", base: "b.txt", name: "b", ext: ".txt" }), "C:\\a\\b.txt"],
      ["win.formatRoot", () => WIN.format({ root: "C:\\", base: "b.txt" }), "C:\\b.txt"],
      ["win.formatNoDir", () => WIN.format({ name: "b", ext: "txt" }), "b.txt"],
      ["win.basenameUNC", () => WIN.basename("\\\\server\\share\\a\\b.txt", ".txt"), "b"],
      ["win.basenameTrail", () => WIN.basename("C:\\a\\b\\"), "b"],
      ["win.dirnameDrive", () => WIN.dirname("C:/a/b"), "C:/a"],
      ["win.dirnameRoot", () => WIN.dirname("C:\\a"), "C:\\"],
      ["win.dirnameUNC", () => WIN.dirname("\\\\server\\share\\a\\b"), "\\\\server\\share\\a"],
      ["win.extnameDot", () => WIN.extname("C:\\a\\.bashrc"), ""],
      ["win.extnameTrail", () => WIN.extname("C:\\a\\b."), "."],
      ["posix.parseDot", () => POSIX.parse("/a/.bashrc"), { root: "/", dir: "/a", base: ".bashrc", ext: "", name: ".bashrc" }],
      ["posix.format", () => POSIX.format({ dir: "/a", base: "b.txt" }), "/a/b.txt"],
      ["posix.basenameEmpty", () => POSIX.basename(""), ""],
      ["posix.dirnameEmpty", () => POSIX.dirname(""), "."],
      ["posix.extnameDot", () => POSIX.extname(".bashrc"), ""],
      ["sep", () => [path.sep, POSIX.sep, WIN.sep], ["\\", "/", "\\"]],
      ["delimiter", () => [path.delimiter, POSIX.delimiter, WIN.delimiter], [";", ":", ";"]],
      ["isAbsolute", () => [POSIX.isAbsolute("/a"), WIN.isAbsolute("C:\\a"), WIN.isAbsolute("C:a"), WIN.isAbsolute("\\\\s\\s")], [true, true, false, true]],
    ];

    for (const [name, fn, want] of cases) P(name, fn(), want);

    // Typed-entry reachability: call `__pirPath` directly for every op and
    // every flavor, plus the shape-mismatch contract.
    const nativePath = (globalThis as any).__pirPath;
    if (typeof nativePath !== "function") {
      failures.push("__pirPath is not installed");
    } else {
      P("native.join.win", nativePath(0, 0, ["a", "b", "..", "c"]), "a\\c");
      P("native.join.posix", nativePath(0, 1, ["a", "b"]), "a/b");
      P("native.resolve.win", nativePath(1, 0, ["C:\\a", "b"]), "C:\\a\\b");
      P("native.resolve.posix", nativePath(1, 1, ["/a", "b"]), "/a/b");
      P("native.normalize.win", nativePath(2, 0, "C:\\CON"), "C:\\CON");
      P("native.normalize.posix", nativePath(2, 1, "a//b"), "a/b");
      P("native.relative.win", nativePath(3, 0, "C:\\a\\b", "C:\\a\\c"), "..\\c");
      P("native.relative.posix", nativePath(3, 1, "/a/b", "/a/c"), "../c");
      P("native.shape.mismatch", nativePath(0, 0, "not-an-array"), undefined);
    }

    // Prove the shim routes every namespace/op through the typed entry (not
    // merely that the entry works when called directly).
    if (typeof nativePath === "function") {
      let nativeCalls = 0;
      (globalThis as any).__pirPath = function (...args: any[]) {
        nativeCalls++;
        return nativePath.apply(globalThis, args);
      };
      try {
        path.join("a", "b");
        path.resolve("a", "b");
        path.normalize("a//b");
        path.relative("/a/b", "/a/c");
        POSIX.join("a", "b");
        POSIX.resolve("a", "b");
        POSIX.normalize("a//b");
        POSIX.relative("/a/b", "/a/c");
        WIN.join("a", "b");
        WIN.resolve("a", "b");
        WIN.normalize("a\\\\b");
        WIN.relative("C:\\a\\b", "C:\\a\\c");
      } finally {
        (globalThis as any).__pirPath = nativePath;
      }
      P("native.shimReached", nativeCalls >= 12, true);
    }

    // The same Node-verified table must pass with the typed entry and the
    // generic invoke tier removed: that is exactly the pure-JS fallback.
    const nativeNode = (globalThis as any).__pirNode;
    const fallbackFailures: string[] = [];
    try {
      (globalThis as any).__pirPath = undefined;
      (globalThis as any).__pirNode = undefined;
      for (const [name, fn, want] of cases) {
        let got: any;
        try { got = fn(); } catch (error: any) { got = "ERR:" + (error && error.code); }
        if (JSON.stringify(got) !== JSON.stringify(want)) {
          fallbackFailures.push(name + ": got " + JSON.stringify(got) + ", want " + JSON.stringify(want));
        }
      }
    } finally {
      (globalThis as any).__pirPath = nativePath;
      (globalThis as any).__pirNode = nativeNode;
    }
    if (fallbackFailures.length > 0) {
      failures.push("JS fallback diverged: " + fallbackFailures.join("; "));
    }

    // --- os: memoized constants must equal their Node values, live ops must
    // keep their shape, and tmpdir follows Node's exact env rule.
    P("os.platform", os.platform(), platformValue);
    P("os.arch", os.arch(), process.arch);
    P("os.type", os.type(), platformValue === "win32" ? "Windows_NT" : platformValue === "darwin" ? "Darwin" : "Linux");
    P("os.release", os.release(), "");
    P("os.version", os.version(), "");
    P("os.machine", os.machine(), process.arch === "x64" ? "x86_64" : process.arch === "arm64" ? "arm64" : process.arch);
    P("os.platformStable", os.platform() === os.platform() && os.type() === os.type(), true);
    const endianProbe = new Uint8Array(new Uint16Array([1]).buffer);
    P("os.endianness", os.endianness(), endianProbe[0] === 1 ? "LE" : "BE");
    P("os.availableParallelism", typeof os.availableParallelism(), "number");
    P("os.EOL", os.EOL, platformValue === "win32" ? "\r\n" : "\n");
    P("os.devNull", os.devNull, platformValue === "win32" ? "\\\\.\\nul" : "/dev/null");
    P("os.EOL.readonly", (Object.getOwnPropertyDescriptor(os, "EOL") as any).writable, false);

    P("os.hostname.type", typeof os.hostname(), "string");
    P("os.hostname.nonEmpty", os.hostname().length > 0 && os.hostname() === os.hostname(), true);
    P("os.homedir.type", typeof os.homedir(), "string");
    P("os.totalmem.type", typeof os.totalmem(), "number");
    P("os.freemem.type", typeof os.freemem(), "number");
    P("os.uptime.type", typeof os.uptime(), "number");
    const la = os.loadavg();
    P("os.loadavg.shape", Array.isArray(la) && la.length === 3 && la.every((v: any) => typeof v === "number"), true);
    const cpuList = os.cpus();
    const cpu0: any = cpuList[0] || {};
    P("os.cpus.shape", [
      Array.isArray(cpuList) && cpuList.length >= 1,
      typeof cpu0.model,
      typeof cpu0.speed,
      typeof cpu0.times,
      typeof (cpu0.times || {}).user,
      typeof (cpu0.times || {}).irq,
    ], [true, "string", "number", "object", "number", "number"]);
    const nics = os.networkInterfaces();
    P("os.networkInterfaces.shape", typeof nics === "object" && nics !== null && !Array.isArray(nics), true);
    const ui = os.userInfo();
    P("os.userInfo.keys", Object.keys(ui).sort(), ["gid", "homedir", "shell", "uid", "username"]);
    P("os.userInfo.types", [typeof ui.uid, typeof ui.gid, typeof ui.username, typeof ui.homedir], ["number", "number", "string", "string"]);

    if (platformValue === "win32") {
      const env = process.env as any;
      const rawTmp = env.TEMP || env.TMP || ((env.SystemRoot || env.windir) + "\\temp");
      const expectedTmp = rawTmp.length > 1 && rawTmp[rawTmp.length - 1] === "\\" && rawTmp[rawTmp.length - 2] !== ":"
        ? rawTmp.slice(0, -1)
        : rawTmp;
      P("os.tmpdir.rule", os.tmpdir(), expectedTmp);
      const tmp = os.tmpdir();
      P("os.tmpdir.noTrailingSep", tmp.length <= 1 || tmp[tmp.length - 1] !== "\\" || tmp[tmp.length - 2] === ":", true);

      const savedTemp = env.TEMP;
      const savedTmp = env.TMP;
      try {
        env.TEMP = "C:\\pir-tmp\\\\";
        P("os.tmpdir.strip", os.tmpdir(), "C:\\pir-tmp\\");
        env.TEMP = "C:\\";
        P("os.tmpdir.driveRoot", os.tmpdir(), "C:\\");
        env.TEMP = "";
        env.TMP = "D:\\pir-tmp";
        P("os.tmpdir.tmpFallback", os.tmpdir(), "D:\\pir-tmp");
      } finally {
        if (savedTemp === undefined) delete env.TEMP; else env.TEMP = savedTemp;
        if (savedTmp === undefined) delete env.TMP; else env.TMP = savedTmp;
      }
    }

    (globalThis as any).__pathOsParity = { checked, failures };
  });
};
"##;

#[tokio::test(flavor = "current_thread")]
async fn path_os_parity() {
    let dir = crate::common::unique_temp_path("path-os-parity");
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let rt = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
    rt.seed_module("pir-ext:path-os-parity.ts", PATH_OS_PARITY_TS)
        .expect("seed path-os fixture");
    let loaded = rt.seeded_load("pir-ext:path-os-parity.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture must load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in path/os parity test: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__pathOsParity)")
        .await
        .expect("read path/os parity results");
    assert_eq!(v["failures"], json!([]), "path/os parity failures: {v}");
    let checked = v["checked"].as_u64().unwrap_or(0);
    assert!(
        checked >= 100,
        "expected a broad path/os corpus, checked only {checked}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// fs parity corpus. Every expectation below was produced by running the same
// calls through Node v24.22.1 on win32 (the posix branches use the documented
// Linux errno values). It covers the audit's destructive findings: exclusive
// create flags, COPYFILE_EXCL/FICLONE*, readdir encodings, BigIntStats,
// empty-path semantics, mkdtemp relative prefixes and the OS error shapes.
// ---------------------------------------------------------------------------

const FS_PARITY_TS: &str = r##"
import fs from "node:fs";
import path from "node:path";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const failures: string[] = [];
    let checked = 0;
    const WIN = (globalThis as any).__pirPlatform === "win32";
    const cwd = process.cwd();
    const abs = (name: string) => path.join(cwd, name);
    const E = WIN
      ? { enoent: -4058, enotdir: -4052, eexist: -4075, eisdir: -4068,
          eperm: -4048, eacces: -4092, ebadf: -4083, einval: -4071, enosys: -4054 }
      : { enoent: -2, enotdir: -20, eexist: -17, eisdir: -21,
          eperm: -1, eacces: -13, ebadf: -9, einval: -22, enosys: -38 };

    const text = (value: any): string => {
      if (typeof value === "bigint") return String(value) + "n";
      try { return JSON.stringify(value); } catch { return String(value); }
    };
    const same = (a: any, b: any) => {
      if (typeof a === "bigint" || typeof b === "bigint") return a === b;
      return JSON.stringify(a) === JSON.stringify(b);
    };
    const eq = (label: string, got: any, want: any) => {
      checked++;
      if (!same(got, want)) failures.push(label + ": got " + text(got) + ", want " + text(want));
    };
    const errOf = (error: any) => ({
      name: error.name, code: error.code, errno: error.errno,
      syscall: error.syscall, path: error.path, message: error.message,
    });
    const throws = (label: string, fn: () => any, want: any) => {
      checked++;
      try { fn(); failures.push(label + ": did not throw, want " + text(want)); }
      catch (error: any) {
        const got = errOf(error);
        if (!same(got, want)) failures.push(label + ": got " + text(got) + ", want " + text(want));
      }
    };
    const osError = (code: string, errno: number, syscall: string, p: any, message: string) =>
      ({ name: "Error", code, errno, syscall, path: p, message });
    const enoent = (syscall: string, p: string) =>
      osError("ENOENT", E.enoent, syscall, p,
        "ENOENT: no such file or directory, " + syscall + " '" + p + "'");

    const reset = (name: string, content: string | undefined) => {
      const p = abs(name);
      try { fs.rmSync(p, { force: true, recursive: true }); } catch {}
      if (content !== undefined) fs.writeFileSync(p, content);
      return p;
    };

    // --- exclusive / open flags -------------------------------------------
    const eexistOpen = (p: string) =>
      osError("EEXIST", E.eexist, "open", p, "EEXIST: file already exists, open '" + p + "'");
    for (const flag of ["wx", "wx+", "xw", "xw+", "ax", "ax+", "xa", "xa+"]) {
      const p = reset("flag-" + flag.replace("+", "p") + ".txt", "OLD");
      throws("writeFileSync existing flag=" + flag,
        () => fs.writeFileSync(p, "NEW", { flag }), eexistOpen(p));
      eq("writeFileSync existing content flag=" + flag, fs.readFileSync(p, "utf8"), "OLD");
      const missing = reset("flagm-" + flag.replace("+", "p") + ".txt", undefined);
      fs.writeFileSync(missing, "NEW", { flag });
      eq("writeFileSync missing content flag=" + flag, fs.readFileSync(missing, "utf8"), "NEW");
    }

    let p = reset("rplus.txt", "ABCDEFGH");
    fs.writeFileSync(p, "xy", { flag: "r+" });
    eq("r+ writes without truncating", fs.readFileSync(p, "utf8"), "xyCDEFGH");
    p = reset("rplus-missing.txt", undefined);
    throws("r+ missing is ENOENT", () => fs.writeFileSync(p, "xy", { flag: "r+" }), enoent("open", p));

    p = reset("wplus.txt", "ABCDEFGH");
    fs.writeFileSync(p, "xy", { flag: "w+" });
    eq("w+ truncates", fs.readFileSync(p, "utf8"), "xy");

    p = reset("append-a.txt", "OLD");
    fs.writeFileSync(p, "NEW", { flag: "a" });
    eq("writeFile a appends", fs.readFileSync(p, "utf8"), "OLDNEW");
    p = reset("append-aplus.txt", "OLD");
    fs.writeFileSync(p, "NEW", { flag: "a+" });
    eq("writeFile a+ appends", fs.readFileSync(p, "utf8"), "OLDNEW");
    p = reset("append-default.txt", "OLD");
    fs.appendFileSync(p, "NEW");
    eq("appendFileSync default appends", fs.readFileSync(p, "utf8"), "OLDNEW");
    p = reset("append-wx.txt", "OLD");
    throws("appendFileSync wx", () => fs.appendFileSync(p, "NEW", { flag: "wx" }), eexistOpen(p));
    eq("appendFileSync wx content", fs.readFileSync(p, "utf8"), "OLD");
    p = reset("append-w.txt", "OLD");
    fs.appendFileSync(p, "NEW", { flag: "w" });
    eq("appendFileSync w truncates", fs.readFileSync(p, "utf8"), "NEW");
    p = reset("append-zero-flag.txt", "OLD");
    fs.appendFileSync(p, "NEW", { flag: 0 });
    eq("appendFileSync falsy flag defaults to a", fs.readFileSync(p, "utf8"), "OLDNEW");

    p = reset("buffer-wx.txt", "OLD");
    throws("writeFileSync Buffer wx",
      () => fs.writeFileSync(p, Buffer.from("NEW"), { flag: "wx" }), eexistOpen(p));
    eq("writeFileSync Buffer wx content", fs.readFileSync(p, "utf8"), "OLD");
    p = reset("buffer-w.txt", "OLD");
    fs.writeFileSync(p, Buffer.from("NEW"), { flag: "w" });
    eq("writeFileSync Buffer w", fs.readFileSync(p, "utf8"), "NEW");

    p = reset("numeric-rdwr.txt", "ABCDEFGH");
    fs.writeFileSync(p, "xy", { flag: fs.constants.O_RDWR });
    eq("numeric O_RDWR no truncate", fs.readFileSync(p, "utf8"), "xyCDEFGH");
    p = reset("numeric-excl.txt", "OLD");
    throws("numeric O_EXCL create",
      () => fs.writeFileSync(p, "NEW", { flag: fs.constants.O_WRONLY | fs.constants.O_CREAT | fs.constants.O_EXCL }),
      eexistOpen(p));
    if (WIN) {
      p = reset("numeric-trunc.txt", "ABCDEFGH");
      throws("numeric O_TRUNC without O_CREAT",
        () => fs.writeFileSync(p, "xy", { flag: fs.constants.O_WRONLY | fs.constants.O_TRUNC }),
        osError("EINVAL", E.einval, "open", p, "EINVAL: invalid argument, open '" + p + "'"));
    }

    p = reset("flag-r.txt", "AB");
    throws("writeFileSync flag r is EBADF on write",
      () => fs.writeFileSync(p, "CD", { flag: "r" }),
      { name: "Error", code: "EBADF", errno: E.ebadf, syscall: "write",
        message: "EBADF: bad file descriptor, write" });
    eq("writeFileSync flag r leaves content", fs.readFileSync(p, "utf8"), "AB");

    throws("flag zz",
      () => fs.writeFileSync(p, "B", { flag: "zz" }),
      { name: "TypeError", code: "ERR_INVALID_ARG_VALUE",
        message: "The argument 'flags' is invalid. Received 'zz'" });
    throws("flag 5.5",
      () => fs.writeFileSync(p, "B", { flag: 5.5 }),
      { name: "RangeError", code: "ERR_OUT_OF_RANGE",
        message: "The value of \"flags\" is out of range. It must be an integer. Received 5.5" });

    // --- promises equivalents ---------------------------------------------
    p = reset("promise-wx.txt", "OLD");
    let rejected: any = null;
    try { await fs.promises.writeFile(p, "NEW", { flag: "wx" }); }
    catch (error: any) { rejected = errOf(error); }
    eq("promises.writeFile wx", rejected, eexistOpen(p));
    eq("promises.writeFile wx content", fs.readFileSync(p, "utf8"), "OLD");
    p = reset("promise-append.txt", "OLD");
    await fs.promises.appendFile(p, "NEW");
    eq("promises.appendFile default", fs.readFileSync(p, "utf8"), "OLDNEW");

    // --- copyFile flags ----------------------------------------------------
    const src = reset("copy-src.txt", "SRC");
    let dst = reset("copy-dst.txt", "OLD");
    throws("copyFileSync EXCL",
      () => fs.copyFileSync(src, dst, fs.constants.COPYFILE_EXCL),
      osError("EEXIST", E.eexist, "copyfile", src,
        "EEXIST: file already exists, copyfile '" + src + "' -> '" + dst + "'"));
    eq("copyFileSync EXCL dest unchanged", fs.readFileSync(dst, "utf8"), "OLD");
    dst = reset("copy-dst2.txt", undefined);
    fs.copyFileSync(src, dst, fs.constants.COPYFILE_EXCL);
    eq("copyFileSync EXCL missing dest", fs.readFileSync(dst, "utf8"), "SRC");
    dst = reset("copy-dst3.txt", "OLD");
    fs.copyFileSync(src, dst, fs.constants.COPYFILE_FICLONE);
    eq("copyFileSync FICLONE copies", fs.readFileSync(dst, "utf8"), "SRC");
    if (WIN) {
      dst = reset("copy-dst4.txt", "OLD");
      throws("copyFileSync FICLONE_FORCE",
        () => fs.copyFileSync(src, dst, fs.constants.COPYFILE_FICLONE_FORCE),
        osError("ENOSYS", E.enosys, "copyfile", src,
          "ENOSYS: function not implemented, copyfile '" + src + "' -> '" + dst + "'"));
      eq("copyFileSync FICLONE_FORCE dest unchanged", fs.readFileSync(dst, "utf8"), "OLD");
    }
    dst = reset("copy-dst5.txt", "OLD");
    throws("copyFileSync EXCL|FICLONE",
      () => fs.copyFileSync(src, dst, 3),
      osError("EEXIST", E.eexist, "copyfile", src,
        "EEXIST: file already exists, copyfile '" + src + "' -> '" + dst + "'"));

    const missingSrc = abs("copy-missing.txt");
    dst = reset("copy-dst6.txt", "OLD");
    throws("copyFileSync missing src",
      () => fs.copyFileSync(missingSrc, dst),
      osError("ENOENT", E.enoent, "copyfile", missingSrc,
        "ENOENT: no such file or directory, copyfile '" + missingSrc + "' -> '" + dst + "'"));

    const srcDir = abs("copy-srcdir");
    try { fs.rmSync(srcDir, { recursive: true, force: true }); } catch {}
    fs.mkdirSync(srcDir);
    dst = reset("copy-dst7.txt", "OLD");
    throws("copyFileSync src is dir",
      () => fs.copyFileSync(srcDir, dst),
      osError("EPERM", E.eperm, "copyfile", srcDir,
        "EPERM: operation not permitted, copyfile '" + srcDir + "' -> '" + dst + "'"));

    dst = reset("copy-dst8.txt", "OLD");
    throws("copyFileSync mode string",
      () => fs.copyFileSync(src, dst, "1" as any),
      { name: "TypeError", code: "ERR_INVALID_ARG_TYPE", message: "mode must be int32 or null/undefined" });
    throws("copyFileSync mode 8",
      () => fs.copyFileSync(src, dst, 8),
      { name: "RangeError", code: "ERR_OUT_OF_RANGE", message: "mode is out of range: >= 0 && <= 7" });
    throws("copyFileSync mode -1",
      () => fs.copyFileSync(src, dst, -1),
      { name: "RangeError", code: "ERR_OUT_OF_RANGE", message: "mode is out of range: >= 0 && <= 7" });
    if (WIN) {
      dst = reset("copy-dst9.txt", "OLD");
      throws("copyFileSync mode 5.5 coerces to 5",
        () => fs.copyFileSync(src, dst, 5.5),
        osError("ENOSYS", E.enosys, "copyfile", src,
          "ENOSYS: function not implemented, copyfile '" + src + "' -> '" + dst + "'"));
    }
    dst = reset("copy-dst10.txt", "OLD");
    const callbackError: any = await new Promise((resolve) => {
      fs.copyFile(src, dst, fs.constants.COPYFILE_EXCL, (error: any) => resolve(errOf(error)));
    });
    eq("copyFile callback EXCL", callbackError,
      osError("EEXIST", E.eexist, "copyfile", src,
        "EEXIST: file already exists, copyfile '" + src + "' -> '" + dst + "'"));

    // --- readdir encodings -------------------------------------------------
    const rd = abs("rdir");
    try { fs.rmSync(rd, { recursive: true, force: true }); } catch {}
    fs.mkdirSync(rd);
    fs.writeFileSync(path.join(rd, "a.txt"), "A");
    fs.writeFileSync(path.join(rd, "é.txt"), "E");
    eq("readdir default", fs.readdirSync("rdir").sort(), ["a.txt", "é.txt"]);
    eq("readdir buffer",
      fs.readdirSync("rdir", { encoding: "buffer" }).map((buf: any) => buf.toString("hex")).sort(),
      ["612e747874", "c3a92e747874"]);
    eq("readdir hex", fs.readdirSync("rdir", { encoding: "hex" }).sort(),
      ["612e747874", "c3a92e747874"]);
    eq("readdir base64", fs.readdirSync("rdir", { encoding: "base64" }).sort(),
      ["YS50eHQ=", "w6kudHh0"]);
    eq("readdir base64url", fs.readdirSync("rdir", { encoding: "base64url" }).sort(),
      ["YS50eHQ", "w6kudHh0"]);
    eq("readdir latin1", fs.readdirSync("rdir", { encoding: "latin1" }).sort(),
      ["a.txt", "Ã©.txt"]);
    eq("readdir ascii", fs.readdirSync("rdir", { encoding: "ascii" }).sort(),
      ["C).txt", "a.txt"]);
    eq("readdir ucs2", fs.readdirSync("rdir", { encoding: "ucs2" }).sort(),
      ["⹡硴", "꧃琮瑸"]);
    eq("readdir empty encoding is utf8", fs.readdirSync("rdir", { encoding: "" }).sort(),
      ["a.txt", "é.txt"]);
    eq("readdir null encoding is utf8", fs.readdirSync("rdir", { encoding: null }).sort(),
      ["a.txt", "é.txt"]);
    const typed = fs.readdirSync("rdir", { withFileTypes: true });
    eq("dirent keys", Object.keys(typed[0]).sort(), ["name", "parentPath"]);
    eq("dirent parentPath verbatim", typed[0].parentPath, "rdir");
    eq("dirent names", typed.map((entry: any) => entry.name).sort(), ["a.txt", "é.txt"]);
    const typedBuffer = fs.readdirSync("rdir", { withFileTypes: true, encoding: "buffer" });
    eq("dirent buffer names",
      typedBuffer.map((entry: any) => entry.name.toString("hex")).sort(),
      ["612e747874", "c3a92e747874"]);
    eq("dirent buffer keys", Object.keys(typedBuffer[0]).sort(), ["name", "parentPath"]);
    eq("dirent buffer parentPath", typedBuffer[0].parentPath, "rdir");
    eq("readdir string options buffer",
      fs.readdirSync("rdir", "buffer" as any).map((buf: any) => buf.toString("hex")).sort(),
      ["612e747874", "c3a92e747874"]);
    const invalidEncoding = {
      name: "TypeError", code: "ERR_INVALID_ARG_VALUE",
      message: "The argument 'encoding' is invalid encoding. Received 'bogus'",
    };
    throws("readdir invalid encoding",
      () => fs.readdirSync("rdir", { encoding: "bogus" }), invalidEncoding);
    throws("readdir invalid encoding number",
      () => fs.readdirSync("rdir", { encoding: 5 }),
      { name: "TypeError", code: "ERR_INVALID_ARG_VALUE",
        message: "The argument 'encoding' is invalid encoding. Received 5" });
    throws("readdir encoding validated before path",
      () => fs.readdirSync("no-such-dir", { encoding: "bogus" }), invalidEncoding);
    throws("readdir string options invalid",
      () => fs.readdirSync("rdir", "bogus"), invalidEncoding);
    throws("readdir file is ENOTDIR",
      () => fs.readdirSync("copy-src.txt"),
      osError("ENOTDIR", E.enotdir, "scandir", src,
        "ENOTDIR: not a directory, scandir '" + src + "'"));

    // --- stats: number and BigInt shapes ----------------------------------
    const sf = reset("stat.txt", "hello");
    const s = fs.statSync(sf);
    const numericFields = ["dev", "mode", "nlink", "uid", "gid", "rdev", "blksize",
      "ino", "size", "blocks", "atimeMs", "mtimeMs", "ctimeMs", "birthtimeMs"];
    eq("stats keys", Object.keys(s), numericFields);
    eq("stats size", s.size, 5);
    eq("stats nlink", s.nlink, 1);
    eq("stats mode is a regular file", s.mode & 0o170000, 0o100000);
    eq("stats numeric types", numericFields.map((key) => typeof (s as any)[key]),
      numericFields.map(() => "number"));
    eq("stats dates", [s.atime instanceof Date, s.mtime instanceof Date,
      s.ctime instanceof Date, s.birthtime instanceof Date], [true, true, true, true]);
    eq("stats instanceof Stats", s instanceof fs.Stats, true);
    eq("stats isFile", s.isFile(), true);

    const bigFields = numericFields.concat(["atimeNs", "mtimeNs", "ctimeNs", "birthtimeNs"]);
    const big = fs.statSync(sf, { bigint: true });
    eq("bigint keys", Object.keys(big), bigFields);
    eq("bigint types", bigFields.map((key) => typeof (big as any)[key]),
      bigFields.map(() => "bigint"));
    eq("bigint size", big.size, BigInt(5));
    eq("bigint nlink", big.nlink, BigInt(1));
    eq("bigint ms from ns", big.atimeMs, big.atimeNs / BigInt(1000000));
    eq("bigint date getter", big.atime.getTime(), Number(big.atimeMs));
    eq("bigint lazy atime own key", Object.keys(big).indexOf("atime") >= 0, true);
    eq("bigint instanceof Stats", big instanceof fs.Stats, false);
    eq("bigint isFile", big.isFile(), true);
    const lb = fs.lstatSync(sf, { bigint: true });
    eq("lstat bigint size", lb.size, BigInt(5));
    eq("lstat bigint isFile", lb.isFile(), true);
    eq("stat bigint:1 is number", typeof fs.statSync(sf, { bigint: 1 } as any).size, "number");
    eq("stat bigint:false is number", typeof fs.statSync(sf, { bigint: false }).size, "number");
    eq("stat throwIfNoEntry false", fs.statSync(abs("missing-stat"), { throwIfNoEntry: false }), undefined);
    eq("stat throwIfNoEntry false bigint",
      fs.statSync(abs("missing-stat"), { throwIfNoEntry: false, bigint: true }), undefined);
    throws("stat throwIfNoEntry true",
      () => fs.statSync(abs("missing-stat"), { throwIfNoEntry: true }),
      enoent("stat", abs("missing-stat")));
    const fd = fs.openSync(sf, "r");
    try {
      const fb = fs.fstatSync(fd, { bigint: true });
      eq("fstat bigint size", fb.size, BigInt(5));
      eq("fstat bigint types", [typeof fb.ino, typeof fb.dev, typeof fb.mtimeNs],
        ["bigint", "bigint", "bigint"]);
    } finally { fs.closeSync(fd); }

    // --- empty paths / existsSync -----------------------------------------
    eq("existsSync empty", fs.existsSync(""), false);
    eq("existsSync empty buffer", fs.existsSync(Buffer.alloc(0)), false);
    eq("existsSync null", fs.existsSync(null as any), false);
    eq("existsSync number", fs.existsSync(5 as any), false);
    eq("existsSync file", fs.existsSync(sf), true);
    throws("statSync empty", () => fs.statSync(""),
      { name: "Error", code: "ENOENT", errno: E.enoent, syscall: "stat", path: "",
        message: "ENOENT: no such file or directory, stat ''" });
    throws("lstatSync empty", () => fs.lstatSync(""),
      { name: "Error", code: "ENOENT", errno: E.enoent, syscall: "lstat", path: "",
        message: "ENOENT: no such file or directory, lstat ''" });
    throws("readFileSync empty", () => fs.readFileSync(""),
      { name: "Error", code: "ENOENT", errno: E.enoent, syscall: "open", path: "",
        message: "ENOENT: no such file or directory, open ''" });
    throws("readdirSync empty", () => fs.readdirSync(""),
      { name: "Error", code: "ENOENT", errno: E.enoent, syscall: "scandir", path: "",
        message: "ENOENT: no such file or directory, scandir ''" });
    throws("writeFileSync empty", () => fs.writeFileSync("", "x"),
      { name: "Error", code: "ENOENT", errno: E.enoent, syscall: "open", path: "",
        message: "ENOENT: no such file or directory, open ''" });
    throws("appendFileSync empty", () => fs.appendFileSync("", "x"),
      { name: "Error", code: "ENOENT", errno: E.enoent, syscall: "open", path: "",
        message: "ENOENT: no such file or directory, open ''" });
    throws("openSync empty", () => fs.openSync("", "r"),
      { name: "Error", code: "ENOENT", errno: E.enoent, syscall: "open", path: "",
        message: "ENOENT: no such file or directory, open ''" });
    throws("unlinkSync empty", () => fs.unlinkSync(""),
      { name: "Error", code: "ENOENT", errno: E.enoent, syscall: "unlink", path: "",
        message: "ENOENT: no such file or directory, unlink ''" });

    // --- mkdtemp relative prefixes -----------------------------------------
    fs.mkdirSync(abs("mkd"), { recursive: true });
    const relative = fs.mkdtempSync("mkd/tmp-");
    eq("mkdtemp relative shape", /^mkd\/tmp-[a-z0-9]{6}$/.test(relative), true);
    eq("mkdtemp relative created in cwd", fs.statSync(relative).isDirectory(), true);
    const trailing = fs.mkdtempSync("mkd/");
    eq("mkdtemp trailing separator", /^mkd\/[a-z0-9]{6}$/.test(trailing), true);
    const bare = fs.mkdtempSync("");
    eq("mkdtemp empty prefix", /^[a-z0-9]{6}$/.test(bare), true);
    eq("mkdtemp empty prefix in cwd", fs.statSync(bare).isDirectory(), true);
    throws("mkdtemp missing parent", () => fs.mkdtempSync("no-such/tmp-"),
      { name: "Error", code: "ENOENT", errno: E.enoent, syscall: "mkdtemp",
        path: "no-such/tmp-XXXXXX",
        message: "ENOENT: no such file or directory, mkdtemp 'no-such/tmp-XXXXXX'" });
    const absPrefix = (WIN ? abs("mkd").replace(/\\/g, "/") : abs("mkd")) + "/abs-";
    const absMade = fs.mkdtempSync(absPrefix);
    eq("mkdtemp absolute keeps spelling",
      absMade.indexOf(absPrefix) === 0 && absMade.length === absPrefix.length + 6, true);

    // --- OS error shapes ---------------------------------------------------
    const ef = reset("err-file.txt", "x");
    const ed = abs("err-dir");
    try { fs.rmSync(ed, { recursive: true, force: true }); } catch {}
    fs.mkdirSync(ed);
    throws("stat missing", () => fs.statSync("err-nope.txt"),
      enoent("stat", abs("err-nope.txt")));
    throws("lstat missing", () => fs.lstatSync("err-nope.txt"),
      enoent("lstat", abs("err-nope.txt")));
    throws("readFile missing", () => fs.readFileSync("err-nope.txt"),
      enoent("open", abs("err-nope.txt")));
    throws("readdir missing", () => fs.readdirSync("err-nope.txt"),
      enoent("scandir", abs("err-nope.txt")));
    throws("writeFile missing parent", () => fs.writeFileSync("err-no/child.txt", "x"),
      enoent("open", abs("err-no/child.txt")));
    throws("readFile directory", () => fs.readFileSync("err-dir"),
      { name: "Error", code: "EISDIR", errno: E.eisdir, syscall: "read",
        message: "EISDIR: illegal operation on a directory, read" });
    throws("writeFile directory", () => fs.writeFileSync("err-dir", "x"),
      osError("EISDIR", E.eisdir, "open", ed,
        "EISDIR: illegal operation on a directory, open '" + ed + "'"));
    throws("writeFile directory wx", () => fs.writeFileSync("err-dir", "x", { flag: "wx" }),
      osError("EEXIST", E.eexist, "open", ed,
        "EEXIST: file already exists, open '" + ed + "'"));
    throws("writeFile directory a", () => fs.writeFileSync("err-dir", "x", { flag: "a" }),
      { name: "Error", code: "EISDIR", errno: E.eisdir, syscall: "write",
        message: "EISDIR: illegal operation on a directory, write" });
    throws("writeFile directory r+", () => fs.writeFileSync("err-dir", "x", { flag: "r+" }),
      { name: "Error", code: "EISDIR", errno: E.eisdir, syscall: "write",
        message: "EISDIR: illegal operation on a directory, write" });
    throws("appendFile directory", () => fs.appendFileSync("err-dir", "x"),
      { name: "Error", code: "EISDIR", errno: E.eisdir, syscall: "write",
        message: "EISDIR: illegal operation on a directory, write" });
    throws("writeFile directory r", () => fs.writeFileSync("err-dir", "x", { flag: "r" }),
      { name: "Error", code: "EBADF", errno: E.ebadf, syscall: "write",
        message: "EBADF: bad file descriptor, write" });
    const throughFile = abs("err-file.txt/child");
    throws("stat through file",
      () => fs.statSync("err-file.txt/child"),
      WIN ? enoent("stat", throughFile)
          : osError("ENOTDIR", E.enotdir, "stat", throughFile,
              "ENOTDIR: not a directory, stat '" + throughFile + "'"));
    throws("write through file",
      () => fs.writeFileSync("err-file.txt/child.txt", "x"),
      WIN ? enoent("open", abs("err-file.txt/child.txt"))
          : osError("ENOTDIR", E.enotdir, "open", abs("err-file.txt/child.txt"),
              "ENOTDIR: not a directory, open '" + abs("err-file.txt/child.txt") + "'"));
    throws("unlink directory", () => fs.unlinkSync("err-dir"),
      WIN ? osError("EPERM", E.eperm, "unlink", ed,
              "EPERM: operation not permitted, unlink '" + ed + "'")
          : osError("EISDIR", E.eisdir, "unlink", ed,
              "EISDIR: illegal operation on a directory, unlink '" + ed + "'"));
    throws("rmdir file", () => fs.rmdirSync("err-file.txt"),
      WIN ? osError("ENOENT", E.enoent, "rmdir", ef,
              "ENOENT: no such file or directory, rmdir '" + ef + "'")
          : osError("ENOTDIR", E.enotdir, "rmdir", ef,
              "ENOTDIR: not a directory, rmdir '" + ef + "'"));
    const ro = reset("err-readonly.txt", "z");
    fs.chmodSync(ro, 0o444);
    throws("write readonly file", () => fs.writeFileSync(ro, "new"),
      WIN ? osError("EPERM", E.eperm, "open", ro,
              "EPERM: operation not permitted, open '" + ro + "'")
          : osError("EACCES", E.eacces, "open", ro,
              "EACCES: permission denied, open '" + ro + "'"));
    fs.chmodSync(ro, 0o666);
    if (WIN) {
      throws("realpath missing", () => fs.realpathSync("err-nope.txt"),
        enoent("lstat", abs("err-nope.txt")));
      const forward = abs("err-fwd-missing.txt").replace(/\\/g, "/");
      throws("absolute forward slashes normalize",
        () => fs.statSync(forward), enoent("stat", abs("err-fwd-missing.txt")));
    }

    // --- readFile options --------------------------------------------------
    eq("readFile empty encoding is Buffer",
      Buffer.isBuffer(fs.readFileSync(sf, { encoding: "" })), true);
    eq("readFile empty encoding length", fs.readFileSync(sf, { encoding: "" }).length, 5);
    throws("readFile options number", () => fs.readFileSync(sf, 5 as any),
      { name: "TypeError", code: "ERR_INVALID_ARG_TYPE",
        message: "The \"options\" argument must be one of type string or object. Received type number (5)" });
    throws("writeFile options number", () => fs.writeFileSync(sf, "x", 5 as any),
      { name: "TypeError", code: "ERR_INVALID_ARG_TYPE",
        message: "The \"options\" argument must be one of type string or object. Received type number (5)" });
    const enc = reset("enc.txt", "A");
    fs.writeFileSync(enc, "42", "hex");
    eq("writeFile string option is encoding", fs.readFileSync(enc, "utf8"), "B");

    globalThis.__fsParity = { checked, failures };
  });
};
"##;

#[tokio::test(flavor = "current_thread")]
async fn fs_parity() {
    let dir = crate::common::unique_temp_path("fs-parity");
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let rt = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
    rt.seed_module("pir-ext:fs-parity.ts", FS_PARITY_TS)
        .expect("seed fs-parity fixture");
    let loaded = rt.seeded_load("pir-ext:fs-parity.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture must load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in fs parity test: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__fsParity)")
        .await
        .expect("read fs parity results");
    assert_eq!(v["failures"], json!([]), "fs parity failures: {v}");
    let checked = v["checked"].as_u64().unwrap_or(0);
    assert!(
        checked >= 100,
        "expected a broad fs corpus, checked only {checked}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
