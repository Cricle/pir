# node_tests — Deno 式 Node 官方测试套件兼容率度量（pir）

用 Node.js 官方 lib 测试套件（`test/parallel`）的一个纯 JS 子集，客观度量 pir
内嵌 QuickJS 运行时的 Node 兼容率。方法学照搬 Deno 的
`tests/node_compat/`（allow-list config + vendored 套件 + 逐文件 pass/fail）。

规模（2026-10-13 第二波扩量后）：**128 个测试文件**（27 个模块）+ 4 个 `common/`
基础设施，取自 nodejs/node `v24.19.0` `test/parallel`。

## 目录结构

```
scripts/node_tests/
  runner.mjs          编排器：改写 require → 生成 work/ 副本 → 逐文件调引擎 → 分类 → 报告
  MANIFEST.txt        vendored 文件清单（runner 的输入；# 注释行）
  vendored.sha256     vendored/** 的 SHA-256 基线（完整性门输入；sha256sum -c 可逐行核对）
  vendored/           nodejs/node v24.19.0 原始测试文件，逐字节未改
    common/           上游 test/common/{index,fixtures,tmpdir,hijackstdio}.js（基础设施）
    test-*.js         上游 test/parallel/ 下的 lib 单元测试
  shims/              harness 自身的基础设施（不属于被测面）
    test.cjs                      node:test 最小实现（test/describe/子测试/skip/todo，
                                  结束时输出 __NODERESULT__ 计数行）
    internal_test_binding.cjs     internal/test/binding 桩（JSStream/previewEntries）
    internal_event_target.cjs     internal/event_target 桩（kEvents 符号）
  work/               运行期生成（test/parallel 布局 + common/shims 副本），勿入库
  REPORT.md           pir 结果报告（runner 生成 + 底部人工分析）
  REPORT-control.md   系统 node 控制组报告（验证 harness 本身）
  CANDIDATES.md       选材记录：控制组可过但未入选的候选 + 排除理由（换 tag 重选材入口）
  （diagnostics-channel 扩量波的逐次报告已归档到 `logs/phase2/node-suite-20261015/`，
  不在此目录重复保留。）
```

## 复跑

```bash
# pir 侧（自动找 target-alt/debug → target-alt/release → target/debug/deps → target/release → target/debug，debug 优先）：
node scripts/node_tests/runner.mjs
#   可选：--pir <pir.exe>   指定二进制（js_mode 需在构建里；target/release、
#                          target/debug 的旧构建没有 pir js，会退化成 agent 模式，
#                          runner 的 PROBE 会报错）
#   可选：--only test-path.js,...   单跑若干文件
#   可选：--out <file.md>   报告输出路径（默认 scripts/node_tests/REPORT.md）
#   自动：完整性门（vendored.sha256 校验，--node 控制组一并受门，见「完整性门」）
#   可选：--check-hashes   只做完整性校验后退出（0/1；不需要 pir 二进制）
#   可选：--skip-hash-check  跳过完整性校验（仅限本地诊断，见「完整性门」）

# 控制组（系统 node，验证 harness）：
node scripts/node_tests/runner.mjs --node --out REPORT-control.md
```

退出码 0；报告是产物。单文件调试：改 `vendored/` 前先看
`work/test/parallel/<name>.js`（改写后的实际运行副本）。

## 改写与补丁（全部记录在案）

vendored 文件逐字节不改；runner 在生成 `work/` 副本时做两类适配
（相当于 Deno 的最小适配层）：

1. require 改写：`../common` → `./common/index.js`（副本布局）、
   `../common/<x>`（tmpdir/hijackstdio 等子模块）→ `./common/<x>.js`
   （含一处上游写法 `../../test/common/<x>`）、
   `node:test` / `test` → `./shims/test.cjs`、
   `internal/test/binding` / `internal/event_target` → 对应桩。
   复制进 work 目录是因为 pir `js` 模式的 fs 根围栏只放行脚本目录树。
2. `common/index.js` 3 处补丁（见 runner.mjs `COMMON_PATCHES`）：
   - `process.versions.openssl ?? true`（pir 不报 openssl 但 node:crypto 可用）
   - `process.config.variables || {}`（pir 的 process.config 是空壳）
   - `getCallSites()[1]` 空帧兜底（mustCall 失败报告自身不能崩）
3. staged `common/` 副本里 helper 的自引用（`../common`、`./`）→ `./index.js`
   （见 runner.mjs `COMMON_SELF_REWRITES`；common 在 work 里比测试文件深一层）。

排除与 harness 预期失败（Deno config.jsonc 的 `ignore`/`reason` 等价物）
分别在 runner.mjs 的 `EXCLUDED`、`HARNESS_EXPECTED_FAIL`，均带中文理由；
未入选的候选文件与排除原因另有 CANDIDATES.md。

单文件超时 `PER_FILE_TIMEOUT_MS`（runner.mjs）：默认 20s，只对纯吞吐型用例放宽，
断言不放松。当前两条：test-performance-many-marks.js（1e6 次 mark，~19s）、
test-buffer-indexof.js（1e6 次 indexOf + 1MB pattern 搜索，26s）——QuickJS 的分配/扫描
速率约为 V8 的 1/10，"跑完但慢"会被默认超时误报成挂起。真挂起的用例不给放宽
（test-async-hooks-execution-async-resource.js 用 60s 试跑仍超时，保持默认）。

## 完整性门（vendored 字节级校验，2026-10-14 加）

**动因**：runner 每一轮都把 `vendored/` 改写成 `work/test/parallel/` 副本，
`--node` 控制组跑的**也是同一份副本**。因此"直接改 `vendored/test-*.js`
（例如删掉一条断言）"会让 pir 侧的红消失，而控制组同样通过——控制组根本
证明不了 vendored 文件没被动过。此前只有"逐字节不改"的口头纪律，机制上无门。

**基线**：`vendored.sha256` —— `vendored/**` 每个文件（含 `common/`）的
SHA-256，GNU `sha256sum` 格式（`<64位hex><两空格>vendored/<相对路径>`，
132 行，按路径序）。哈希来源：上游 nodejs/node `v24.19.0` 的 test/ 原文件
（`test/parallel/*.js` + `test/common/{index,fixtures,tmpdir,hijackstdio}.js`）。
`work/` 是运行期产物，**不在**哈希范围。

仓库记录的三处 `common` 补丁（runner.mjs `COMMON_PATCHES`）是 **stage `work/`
时的改写，不写回 `vendored/`**——被哈希的是未改写的上游原文件。三处如实列出：

1. `process.versions.openssl` → `process.versions.openssl ?? true`
   （pir 不报 openssl 但 node:crypto 可用；不补会让 buffer 测试静默走 no-crypto 路径）
2. `process.config.variables` → `(process.config.variables || {})`
   （pir 的 process.config 是空壳，hasQuic 探测会崩）
3. `getCallSites()[1]` → `(getCallSites()[1] || { scriptName: '(unknown)', lineNumber: 0 })`
   （帧数不确定时防 mustCall 失败报告自身崩溃）

**校验**（两条等价，第一条逐行可手工核对）：

```bash
cd scripts/node_tests
sha256sum -c vendored.sha256      # 132 行逐行 OK
node runner.mjs --check-hashes    # 只做门校验后退出 0/1，不需要 pir 二进制
```

每次 `runner.mjs` 运行（**含 `--node` 控制组**）在 stage `work/` 之前自动
执行同一校验；任何不一致 → 非零退出，并逐项列出「文件 + 期望哈希 + 实际
哈希 + 提示」，拒绝运行。文件缺失、以及 vendored/ 下出现未登记文件，同样报错
（扩量/换 tag 后基线要同步重算，见下）。

**逃生门**：`--skip-hash-check`。只允许本地诊断（临时做 vendored 实验、复现上游
行为），运行时 stderr 会打印醒目警告；**任何对外报告/结论都必须走完整校验**，
跳过校验的轮次不得被引用。

**重算基线**（换 tag / 扩量 / 有意改 vendored 时；属于受审变更，`vendored.sha256`
的 diff 必须逐条解释）：

```bash
cd scripts/node_tests
node -e 'const fs=require("fs"),p=require("path"),c=require("crypto");const w=(d,r="",a=[])=>{for(const e of fs.readdirSync(d,{withFileTypes:true})){const q=r?r+"/"+e.name:e.name;e.isDirectory()?w(p.join(d,e.name),q,a):a.push(q)}return a};const f=w("vendored").sort();fs.writeFileSync("vendored.sha256",f.map(r=>c.createHash("sha256").update(fs.readFileSync(p.join("vendored",r))).digest("hex")+"  vendored/"+r).join("\n")+"\n");console.log("hashed "+f.length)'
sha256sum -c vendored.sha256      # 复验（文件须 LF；CRLF 会让 sha256sum -c 报路径错误）
```

## 当前结论（2026-10-14 全量复认证，assert/行号保真 + 引擎边界收敛波）

- **认证：2026-10-14 12:31 全量复跑**（binary `target-alt/debug/pir.exe`，同日构建；
  `SUMMARY: files=126 pass=122 fail=3 harnessFail=1 excluded=2`）。
- pir（**口径 target-alt/debug**，含 `pir js`）：**122/125 (97.6%)**；子测试级
  **226 pass / 3 fail / 8 skip**（98.7%）；harness 预期失败 1（`test-util-inspect.js`，控制组
  node 同样失败）；排除 2。全量以 `REPORT.md` 为准（含底部「人工分析」）。
- 本波覆盖：① `node:assert` 生成消息与调用点恢复（V8 `getErrorSourcePositions` 等价物——
  自写 tokenizer + CallSite 行列 + 结构化链匹配；`new Function` 源码经 prelude 注册表登记），
  test-assert.js 由 6/18 收敛到 16/2；② 行号保真管道（纯 JS 直通不重印 + CJS eval wrapper
  同行拼接），**test-util-format 由 harness 预期失败转为真通过**（runner 里该文件的旧
  `HARNESS_EXPECTED_FAIL` 条目已删）。
- **剩余 3 红全部是引擎文案/能力边界**（控制组 node v24.19.0 通过，故不计入 harness
  预期失败；逐条 repro 见下）。shim 层无不失真的修法——彻底对齐需 fork/patch
  `rquickjs-sys` 的 `quickjs.c`（见下节末）。
- 复验轮（同日，`target-alt/debug` 11:05 构建）：三条红的根因逐条重跑复现，结论不变，
  **无 shim 改动**（数字与上一轮一致：122/125、226/3/8、排除 2、harness 预期失败 1）；
  下节补记本轮拿到的一手依据。
- 全量复认证（同日 12:31，`target-alt/debug/pir.exe` 12:14 构建，含期间新增改动
  `host_functions/buffer_ops.rs`、`extensions/api/shims.rs`）：**逐项与上一轮相同**
  （files 122/125、subtests 226/3/8、排除 2、harness 预期失败 1）；三条红仍为引擎
  文案/能力边界，**本轮无 shim 改动**，逐条归因见 `REPORT.md` 底部「人工分析」。

### 引擎边界（3 条红的根因，2026-10-14 定案）

| 文件 | 唯一失败点 | node/V8 | QuickJS（pir） | 判据 |
|---|---|---|---|---|
| `test-buffer-alloc.js` | `new Uint8Array(2**53)` 的 RangeError 文案 | `Invalid typed array length: 9007199254740992` | `invalid array index`（`JS_ToIndex` 固定串，quickjs.c:14493-14502） | 删掉该 4 行断言 → 整文件 exit 0（其余 1191 行全过） |
| `test-assert-deep.js` | `Check proxies`：ownKeys 不变量 TypeError 文案 | `'ownKeys' on proxy: trap result did not include 'length'`（带缺失键名） | `proxy: target property must be present in proxy ownKeys`（quickjs.c:52801 固定串，无键名；链路拿不到 proxy target/trap） | 删掉该断言 → 53 pass / 0 fail |
| `test-assert.js` | (a) direct eval 无源；(b) vm 无 realm | (a) V8 把错误点源码行直接交给 JS；(b) realm 真隔离 | (a) QuickJS 无该能力，eval 帧报"外层文件 + eval 相对行列"，读盘会切出错源码；拦 eval 会把直接 eval 变间接（语义破坏）；(b) vm shim 同 realm（`vm_shim.js.txt` 头注明永久边界） | 逐条 repro + 删掉即绿：16/2 → 17/1 → 18/0 |

彻底对齐的正确路径（**本轮不做**）：`quickjs.c:14500 JS_ToIndex` 按调用点分文案
（typed array ctor 61814 / ArrayBuffer ctor 59222 / DataView ctor 61939）、`52801` 把
缺失键名（循环变量 `tab2[i].atom`）写进 V8 格式串。eval 无源 / vm realm 不在此路（需
V8 级 API / 多 context 暴露）。已评估过的 shim 替代（包全局 `Uint8Array` 改文案）：
实测 `new Uint8Array(16)` +20%~+220%、`Buffer.allocUnsafe` +81%，且只会形成"半 V8 化"
文案面 → 不做。

本轮复验新增的一手依据（结论同上表，均为引擎侧事实，非推断）：

- 三条"删掉即绿"的判据本轮亲自重跑：`test-buffer-alloc.js` 注释掉 14-17 行 → exit 0；
  `test-assert-deep.js` 注释掉 1151-1160 行（`invalidTrap` + ownKeys 断言）→ 53/0；
  `test-assert.js` 注释掉 eval 断言（1030-1036）与 vm 断言（1480-1492）→ 18/0。
  即三个文件内**无其它可修失败**，唯一阻塞点就是上表那三条。
- eval 无源的引擎级证明：`quickjs.c:37760-37761` 只在**函数字面量**解析时保存
  `fd->source`（`JSFunctionBytecode.source`，`Function.prototype.toString` 的唯一来源
  `42626-42628`）；eval 脚本/模块体是顶层 bytecode，`b->source == NULL`，实测对 eval
  帧的 `getFunction()` 做 `Function.prototype.toString` 得到
  `function <eval>() { [native code] }`，帧文件名是占位符 `<input>`、行列是 eval 内相对
  位置。引擎不留源码 → JS 侧无法还原（从调用点字面量反推需对任意表达式二次求值，
  不安全，不做）。
- vm realm：引擎支持同一 `JSRuntime` 下多 `JSContext`（rquickjs `Context::full` →
  `JS_NewContext`），但暴露给 JS shim 需保留对象身份的宿主桥（现有 `__pir_host` 是
  JSON-only）——属运行时特性，不是 shim 改动。评估过的"伪 realm"（每 context 包一层
  错误构造器 facade）会让沙箱内"引擎抛的错误是宿主原型、new 出来的错误是 facade 原型"
  自相矛盾，比同 realm 更差 → 不做。
- `test-util-inspect.js` 的 harness 预期失败口径本轮用控制组复验：
  `runner.mjs --node --only test-util-inspect.js` 仍 fail（node v24.19.0 同样过不了
  JSStream stub），见 `REPORT.partial.md`。（注：pir 在该文件里更早失败于
  `util.inspect` 的 async generator 函数 tag 文案 `[Function: abc] AsyncGeneratorFunction`
  ≠ node 的 `[AsyncGeneratorFunction: abc]`——这是被 harness 口径遮住的一条真实 shim
  差异，不影响计分，本轮不动。）

## 历史结论（2026-10-13，第二波扩量：88 → 128 文件）

- 控制组 node v24.19.0（Windows x64）：**125 通过 / 0 失败**（+1 harness 预期失败，
  `test-util-inspect.js`）；本波新增 40 个文件控制组全绿。
- pir（target-alt/release，含 `pir js` 脚本运行器）：**40/124 (32.3%)**；
  子测试级 pass 149 / fail 80（65.1%）。
- 拆分：老 84 个计分文件 31 通过（与上一版持平，无回归）；本波新增 40 个里
  **9 通过 / 31 失败**（通过：console-assign-undefined、console-log-throw-primitive、
  diagnostics-channel-has-subscribers、event-emitter-emit-context、
  perf-hooks-timerify-return-value、stream-destroy-event-order、
  timers-clear-null-does-not-throw-error、url-domain-ascii-unicode、url-format-whatwg）。
- 本波选材口径：优先补覆盖矩阵里的弱模块（async_hooks / diagnostics_channel /
  perf_hooks / url / zlib / console / module / stream / events / process / timers），
  候选全部先过控制组再入 MANIFEST（Deno 口径不变）。
- 新暴露的失败簇：`class constructors must be invoked with 'new'`（zlib/crypto 的
  构造器在 shim 里可无 new 调用，2 文件）、`performance.mark/measure` 缺失
  （test-performance-many-marks / measure-detail）、`diagnostics_channel.bindStore`
  缺失、`process.cpuUsage` throws 档、`zlib.constants` 可写、fs.constants 数值不一致、
  module `_nodeModulePaths/_globalPaths` 返回值差异、`test-url-fileurltopath` 断言。
  逐条根因见 REPORT.md 底部「人工分析」。

## 下一步（按投入产出排序，2026-10-13 扩量波后更新）

1. `node:assert` 本体（6/6 个官方 assert 测试失败，子测试 fail 69 个）：
   报错形状/消息模板（ERR_INVALID_ARG_TYPE 等 code 字段、`assert.throws`
   的 expected/actual 比对）与 `assert.partialDeepStrictEqual`。
2. 各 shim 的**参数校验缺失**（`Missing expected exception`: fs/buffer/os/
   process/util/async_hooks ≈10 文件）——按 Node 的 `internal/errors` 语义抛
   TypeError/RangeError + code。
3. console 面：`Console` 构造器、`count/resetCount/group/groupEnd/table`、
   方法 `.name` 不被 bound 化（4 文件）。
4. 编码长尾：Buffer `base64url`、`latin1` 别名；string_decoder 全编码族。
5. zlib / crypto shim 内部错误：`zlib.*Sync` 返回值、`zlib.crc32` 形状、
   `crypto` 里无 `new` 调用 Hmac（`class constructors must be invoked with 'new'`）。
6. fs：`mkdtemp` EACCES（沙箱）、`opendir`、`os.EOL` 只读、`lchmod` 仅 macOS
   才应暴露（Windows 上 pir 暴露后走了 macOS 分支）、错误消息不要走系统 locale。
7. perf_hooks：`performance.mark/measure`。
8. 常规化：每次 js_mode/兼容层 PR 后跑一遍（分钟级），把 pass 数写入
   CHANGELOG；与 eco_census 的 95% 目标并行汇报（ecosystem% + node-suite%）。
9. 继续扩量：CANDIDATES.md 里还有 222 个控制组可过的文件（第二波取走 40 个，
   覆盖 async_hooks / diagnostics_channel / perf_hooks / url / zlib / console /
   module / stream / events / process / timers）；换 Node tag 时改 runner.mjs 的
   `NODE_TAG` 后重取（MANIFEST.txt 列出全部文件路径）。
10. 引擎级惯用法（不属于本套件分母，但本波实测会打到）：`error.stack` 缺
    `Name: message` 首行、`URLSearchParams` 迭代器不前进（for..of/spread 死循环）、
    `Writable` 缺 own `Symbol.hasInstance`、`PassThrough` 不继承 `Transform`、
    `require("util").types !== require("util/types")` —— 见
    `scripts/eco_census.mjs` 的 IDIOMS 与 `scripts/compat_patterns.mjs`。
