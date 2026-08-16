# 代码审计报告 · PHASE 3（基线 0d765d7 + 工作区未提交改动）

- **基线**：`0d765d7`（master，2026-08 网络工具批：web_read 浏览器渲染接线）+ **工作区未提交改动**（51 文件，+2393/−946，含未跟踪新文件 `crates/core/src/tools/browser_tools.rs`）
- **范围**：全库逐文件（`crates/core` 84 文件 / 约 4.0 万行；`crates/app` 10 文件 / 约 2.8 千行；`crates/sandbox` 2 文件 / 约 1.2 千行 + 集成测试）
- **侧重**：全面审计（安全 / 正确性 / 架构 / 测试 / 依赖），用户确认
- **方法**：只读静态审计；7 路并行子代理分模块深审 + 主审亲读复核所有 HIGH 级锚点；不编译、不改码
- **证据分级**：🟢 = 主审亲读复核确认；🟡 = 引用子代理报告（线号为工作区现行版本）
- **对比基线**：`AUDIT_PHASE2_68DDAC3.md`（68ddac3，2026-08-14）

---

## 0. 执行摘要

**结论：Phase 2 声明的修复绝大多数真实落地（HIGH 9 项中 7 项亲读确认修复），但本轮在「未提交的新代码 + 修复的残留面」上发现 8 项 HIGH，其中 3 项是可直接被外部/恶意网页利用的安全漏洞。**

最紧急的三件事：

1. **H1（安全）`Origin: null` 放行 + 回环免 token + CORS 反射** → 任意网页经沙箱 iframe 即可无凭据读取本机记忆/会话/工具轨迹、注入提示词，并可经 WS `/scene` 窃取审批 id 后代批 `exec_command`（本机 agent 远程操控链）。
2. **H2（安全）core 文件工具（read_file/write_file/delete_file/download_file/list_dir）路径校验纯词法、无 canonicalize** → Phase 2 的 B1（junction 逃逸）只修了 sandbox 子进程，生产主路径的文件工具仍可经 junction/symlink 越界读写删沙箱根外文件。
3. **H3（安全）新增网络工具的浏览器执行路径完全绕过 SSRF 校验**（`web_read render=browser` 直启 headless Chrome 抓取任意 URL；`browser_navigate/open` 重定向每跳无守卫）→ 云元数据 `169.254.169.254` / 内网地址可达。

另有：**H5 PolicyEngine 的文件/输出脱敏策略生产零接线**（SENSITIVE_DENY、密钥脱敏函数存在但从不被生产路径调用）、**H6 沙箱命令白名单/黑名单生产死代码**（`exec_via_sandbox` 从不传 `--allow`，B5 修复形同虚设）、**H7 watchdog 重启后 `wake_inflight` 残留导致唤醒循环永久停摆**、**H8 防重放台账唯一键缺参数维度（写侧）**。

**工作区未提交改动定性**：各子代理逐 hunk 核对一致——绝大多数为 `cargo fmt` 排版、能力声明新增（browser 工具）与依赖挪动（`tokio-tungstenite` dev→正式、新增 `sha2`），**无行为回归**；真正的「新代码」是未跟踪的 `browser_tools.rs`（约 2300 行 CDP 客户端，已接线进工具循环）与 web_tools.rs 的 SSRF/浏览器批，其问题即下文 H3/H4 及若干 MEDIUM。

---

## 1. Phase 2 遗留修复验证（AUDIT_PHASE2 结论 → 本轮状态）

### 1.1 HIGH 级（9 项）

| # | 遗留项 | Phase2 状态 | 本轮状态 | 证据 |
|---|--------|------------|---------|------|
| S1 | 生产 ApprovalGate 未接线 | HIGH | ✅ 已修复 | `service.rs:214-221` `build_executor()` 链含 `.with_approval(approval::global())`；`assemble()` 内 `init_global(tool_root)`（`service.rs:146`）；生产接线单测 `service.rs:1054-1088`；`approval=None` 直通分支仅剩测试可达 🟢 |
| L1 | 防重放键缺参数维度 | HIGH | ⚠️ **读侧已修、写侧未修（H8）** | 读侧：`replay.rs:66-72` + `llm_metrics.rs:258-261` 按 `args_json` 过滤 ✅；写侧：`schema.rs:688` 唯一键仍 `UNIQUE(request_id,round,attempt,tool_name)`，`llm_metrics.rs:221-226` ON CONFLICT 覆盖 → 同轮同名不同参数互相覆盖 🟢 |
| D1 | reminders 时区字典序 | HIGH | ✅ 已修复 | `reminders.rs:33-34` 改 `datetime(due_at) <= datetime(?1)` + `ORDER BY datetime(due_at)`；`extra.rs:162-166` 写入归一 UTC `Z`；测试断言 `+08:00 → UTC 归一`（`extra.rs:405`）🟢 |
| M1 | TICK 前缀无分隔转义 | HIGH | ✅ 已修复 | `messages.rs:257-268` `=== TICK CONTEXT START/END ===` + `sanitize_untrusted` + 1024 字符裁剪 + 测试 🟡 |
| M2 | SectionTag 覆盖缺口 | HIGH | ✅ 已修复（1 处残留见 2.3 L1） | `injector_format.rs:138-160` section_kind 覆盖全部缺口节，测试 `gap_sections_get_memory_tag_and_escape_content` 🟡 |
| A1 | watchdog 假死挂死 | HIGH | ✅ 已修复（连带新问题 H7） | `watchdog.rs:115,166-169` `STUCK_ABORT_GRACE=500ms` 限时等待；测试 `stuck_in_sync_block_supervisor_survives`（`watchdog.rs:248-288`）🟢 |
| A2 | 提醒 consume-before-deliver | HIGH | ✅ 已修复 | `wakeup.rs:97-112` `due_wakeup` 查询不消费；生产 `service.rs:962-982` 交付成功后才 `mark_fired`（`service.rs:696-708`），失败走 `wakeup_dropped`；测试 `due_wakeup_does_not_consume_reminders` 🟢 |
| B1 | sandbox junction 逃逸 | HIGH | ⚠️ **sandbox 内已修，core 文件工具未修（H2）** | `sandbox/main.rs:394-418` canonicalize 父目录为锚点 + 返回真实落点 + 目标链接再解析；测试 `escape_symlink_write_path_new_file_rejected`。但 `tools/mod.rs:771-786` `resolve_under_root` 仍纯词法（生产文件工具不走 sandbox）🟢 |
| B2 | 子进程句柄继承挂死 | HIGH | ✅ 已修复（有界收尾） | `main.rs:162-254` 分读线程 + mpsc 轮询 + `recv_timeout(1500ms)`；Windows `taskkill /T /F`、Unix 进程组 `kill -9 -pgid`；残余：Windows 无 Job Object，孙进程可存活 🟡 |

### 1.2 MEDIUM 级（20 项，Phase2 表内）

| 项 | 本轮状态 | 证据 |
|----|---------|------|
| S2 `global()` 临时目录兜底 | ✅ 已修复（可变槽 + fail-closed 隔离门） | `approval.rs:278-310` `Mutex<Option<Arc>>`，`init_global` 覆盖替换；未初始化时显式 error + 120s 超时拒绝 🟢 |
| S3 approval id 可预测 | ✅ 已修复 | `approval.rs:131-141` 追加进程级 64 位随机熵（非密码学 RNG，见 2.3 L9）🟢 |
| S4 审批回调锁内执行 | ✅ 已修复 | `approval.rs:184-189` 锁内仅 clone Arc，回调锁外调用 🟢 |
| L2 熔断清零计数 | ✅ 已修复 | `tool_loop.rs:557` 收紧到阈值−1，成功由 record_outcome 清零 🟡 |
| L3 flusher unreachable panic | ✅ 已修复 | `metrics.rs:454-460` done 后迟到 CallFailed 仅 debug 日志 🟢 |
| L4 flusher 失败清空 pending | ✅ 已修复 | `metrics.rs:628-673` 失败保留合并回重试、成功才 clear 🟡 |
| L5 pending_ctx 无界 | ✅ 已修复 | `metrics.rs:543-549` `MAX_PENDING_CTX=4096` 满则淘汰旧暂存 🟡 |
| L6 UPSERT 终态守卫 | ✅ 基本修复（乱序小洞见 2.3 L19） | `llm_metrics.rs:146-175` done 覆盖/错误不覆盖成功 + attempt MAX + last_error 只增；`metrics.rs:345-347,418` 内存侧守卫 🟢 |
| D2 唯一索引启动失败 | ✅ 已修复 | `schema.rs:94-145` `ensure_unique_index_dedup` 先查重去重再建索引；真实库三唯一索引实测存在 🟡 |
| D3 FTS 全量重建 | ✅ 已修复 | `schema.rs:872-885` `migration_fts_rebuild_v1` flag 守卫，首次重建后由触发器增量维护 🟡 |
| D4 memories check-then-act | ⚠️ 部分修复 | `memories.rs:35-101` 已包事务 ✅；但 `connection.rs:52` 为 BEGIN **DEFERRED**（非 IMMEDIATE）→ 跨进程竞态仍在，靠部分唯一索引兜底 🟡 |
| M3 TICK 历史升格 System | ✅ 已修复 | `messages.rs:595-599` runtime context 恒为 User role 🟡 |
| M4 信封正则缺 `+` | ✅ 已修复 | `threads.rs:135` 与 `retrieval.rs:54` 统一 `[\d\-T:+]+` 🟡 |
| M5 commitments 无界 | ✅ 已修复 | `threads.rs:441-469` `COMMITMENTS_CAP=256`，只淘汰 closed，open 单例永不淘汰 🟡 |
| A3 裸 spawn 无 panic 守护 | ✅ 已修复 | `service.rs:357-370` 内层 spawn + 外层 await JoinError 转结构化日志 🟢 |
| A4 幂等键只写不查 | ⚠️ 部分修复（TOCTOU 见 2.2 M22） | `service.rs:418-439` 入口校验终态命中即返回 ✅；但 check-then-create 非原子，唯一索引冲突「降级继续」→ 并发/在途重复仍双执行 🟢 |
| R1 闸门与 mark_fired 非原子 | ⚠️ 已串行化，重启路径有缺陷（H7） | `wake_inflight` 串行化 + `due_wakeup` 查询态；但 watchdog 重启后 in-flight 残留永久跳过 🟢 |
| R2 evolution `?` 跳过清理 | ✅ 已修复 | `evolution/mod.rs:127-143` approval_gate 各分支先 `snapshot.cleanup().ok()` 🟡 |
| R3 matter completed 先持久化后校验 | ⚠️ 基本修复（1 处残留见 2.2 M26） | `matter.rs:342-347` additivity 校验前置 ✅；self_verified 分支 `record_signal` 仍在 transition 后（`matter.rs:359-369`）🟡 |
| R4 scene rev 乐观锁 | ✅ 等价修复 | `scene/store.rs:61-71,133-137,290-295` 事件携带锁内 rev/base 快照；服务端权威模型下正确 🟡 |
| B3 read_line 无上限 | ✅ 已修复（软上限） | `main.rs:615-659` `MAX_RPC_LINE_BYTES=256KB` 丢弃超长行协议继续；残余：越过 max 的块内含换行时整行返回（见 2.3）🟡 |
| B4 TOCTOU | ⚠️ sandbox 已修，core 未修 | sandbox `resolve_in_root` 返回真实落点路径 ✅；core `resolve_under_root` 无 canonicalize（H2）🟢 |
| B5 元字符黑名单 | ⚠️ 代码已补齐但生产未启用（H6） | `main.rs:524-532` 已补 `^ ! ( )`；但 `main.rs:107` 白名单为空时整体跳过，生产 `exec_via_sandbox` 从不传 `--allow` 🟢 |

**结论：Phase 2 共 29 项遗留中 23 项确认彻底修复、4 项部分修复（L1 写侧 / D4 / A4 / R3）、2 项修复不完整（B1 core 面 / B5 生产未启用）——后 6 项的残留直接构成本轮 H2/H6/H8 及两个 MEDIUM。**

---

## 2. 新发现问题（Phase 3）

### 2.1 HIGH（8 项，按风险排序）

| # | 位置 | 问题 | 证据 | 建议 |
|---|------|------|------|------|
| H1 | `api/security.rs:111-113` + `api/server.rs:176-220,251-255` | **`Origin: null` 全量放行 → 任意网页可 drive-by 操控本机 agent**。`is_loopback_origin("null")→true`；配合回环免 token + CORS 原样反射（`Access-Control-Allow-Origin: null`），任何网站沙箱 iframe（opaque origin 发送 `Origin: null`）可无凭据：读取 `/events/history`（LLM 日志/会话）、`/trace`（工具参数）、`/conversations`；注入 `POST /message`（30/10s 限流外）；WS `/scene`（`security.rs:139-141` null origin + 回环放行）经 hello 快照拿到审批卡 id（`approval:{id}`），再 `POST /approval allow_once` **代批 exec_command**（`routes.rs:301-325`）→ 任意命令执行链。缓解：Chrome PNA 预检可能拦 public→loopback，但 Firefox/Safari/本地 HTML 不受限。 | 🟢 `is_loopback_origin` null 分支；`server.rs:181-188` origin 校验、`:203-220` 回环免 token、`:251-255` CORS 反射；`scene.rs:99-110` hello 快照含审批卡；`server.rs:73-89` 审批卡写入 scene | HTTP/WS 拒绝 `Origin: null`（或强制 token）；CORS 不对 null 反射；`/scene` 与 `/approval` 增加关联鉴权（token 或一次性握手） |
| H2 | `tools/mod.rs:771-786` `resolve_under_root` | **core 文件工具无 junction/symlink 防护，生产主路径可逃逸沙箱根**。`read_file/write_file/delete_file/list_dir/make_dir/download_file` 全部在 core 进程内执行，路径校验**纯词法**（`normalize_absolute` + `path_prefix_within`），无 canonicalize；sandbox 的 B1 修复只对 exec 的 cwd 生效。LLM 经一次已审批的 `exec_command "mklink /J j <外目录>"`（junction 免管理员）即可在沙箱内种下链接，后续**免审批**的文件读写删越界。SENSITIVE_DENY 亦不生效（见 H5）。 | 🟢 `resolve_under_root` 全文无 canonicalize；`write_file` 直接 `create_dir_all`+`write`（`mod.rs:326-333`）；`exec_via_sandbox` 仅承接 exec（`mod.rs:415-418`） | 移植 sandbox `resolve_in_root` 的父目录 canonicalize + 真实落点校验到 core（两处实现收敛共享）；对已存在目标做链接解析 |
| H3 | `web_tools.rs:443-460` `render_with_browser` | **`web_read render=browser` 完全绕过 SSRF 校验**。浏览器策略零在 `allow_lan=false` 下直接 `Command::new(chrome).arg(url)` 抓取任意 URL，全程无 `check_url_ssrf`；`http://169.254.169.254/latest/meta-data/`、`http://127.0.0.1:8080/admin` 渲染文本原样回喂 LLM。直连 HTTP 路径（`fetch_via_direct`，`web_tools.rs:565`）有校验，浏览器路径缺失。 | 🟢 `web_read_impl` 443-460 无任何 SSRF 判定；`render_with_browser` 1764-1779 直启 chrome | 调 `render_with_browser` 前对 URL 执行 `check_url_ssrf(&parsed, allow_lan)`，失败计入 failures 回落 http |
| H4 | `browser_tools.rs:893-901,1271-1276,1381-1384` | **browser_navigate/browser_open 只校验首跳，重定向每跳无守卫**。`assert_browser_url` 只查导航目标；`Page.navigate` 后 Chrome 自行跟随重定向，公网 URL 302 → `169.254.169.254` → `browser_inspect` 读出元数据（CDP 无 redirect policy）。`browser_tabs new` 同病。 | 🟡 navigate_page 仅 `Page.navigate` + wait_ready_state | 导航后读 `location.href` 复检；或禁用自动重定向逐跳校验；命中私网回 about:blank 并报错 |
| H5 | `policy/mod.rs:203-253` + `tools/mod.rs` 文件工具 | **PolicyEngine 生产零接线：SENSITIVE_DENY 与输出脱敏全部落空**。`check_file_access/check_output_release/check_memory_access/check_network_access` 全库仅测试（`security_regression.rs`、policy 测试）调用；文件工具只做词法根约束，`.ssh/.env/credentials` denylist（`capability::is_path_denied`，`capability/mod.rs:726`）从未在执行层生效；`exec_command` 声明 `output_policy=Sanitize`（`capability/mod.rs:268`）但 stdout/stderr 原样返回（`tools/mod.rs:481-488`），密钥可直进 LLM 上下文与台账。安全回归测试制造「已防护」假象。 | 🟢 grep 全库：4 个 check_* 仅测试调用；`is_path_denied` 仅测试引用；exec_command_inner 无脱敏 | 文件工具执行前接 `is_path_denied`；exec 输出接 `check_output_release`/脱敏；修 docs 与实现对齐 |
| H6 | `tools/mod.rs:502-509` + `sandbox/main.rs:107` | **沙箱命令白名单 + B5 黑名单生产死代码**。`exec_via_sandbox` spawn sandbox 只传 `--root`、从不传 `--allow` → `allow_commands` 恒空 → `main.rs:107` 判定整体跳过 → 元字符黑名单（含 `^ ! ( )`）不生效，`exec_command` 生产语义 = 任意命令 `cmd /C`。威胁模型文档「argv[0] 精确白名单」承诺在生产不成立。 | 🟢 `exec_via_sandbox` 无 `--allow`；`main.rs:107` 空名单跳过；全仓 `--allow` 仅 escape_suite 使用 | app 装配显式注入最小白名单并强制 `--allow`；或把元字符黑名单改为与白名单解耦的无条件生效 |
| H7 | `service.rs:963-977` + `watchdog.rs:166-169` | **watchdog 重启后 `wake_inflight` 永不清空 → 唤醒循环永久停摆**。in-flight 非空即 `continue` 跳过本轮；唯一清空点 `wakeup_delivered/wakeup_dropped` 在被 abort 的 worker 内部，abort 后不执行清理；心跳只在每轮迭代开头 beat 一次，一次 `run_wakeup_turn`（LLM+工具循环，上限 100 轮）超过默认 180s 心跳超时即被误判假死 → abort → 重启后的新 worker 每轮跳过 → 到期提醒静默不再投递直至进程重启。 | 🟢 `service.rs:963-966` 跳过逻辑、`:973-977` 置入、`:707,712` 清空点；`watchdog.rs:166-169` abort 无清理；`:959` 心跳位置 | ① `run_wakeup_turn` 期间持续 beat 或 watchdog 超时按 LLM 轮上限倍数放大；② 重启回调（`on_restart`）清空 `wake_inflight` |
| H8 | `schema.rs:688` + `llm_metrics.rs:221-226` | **防重放台账唯一键缺参数维度（写侧）**。`UNIQUE(request_id,round,attempt,tool_name)` 无 args；同轮两次同名工具（attempt 恒 1）第二次 `ON CONFLICT DO UPDATE` 覆盖首行（args_json/result_json 均被替换）→ 首调用的账丢失；响应丢失重试时 `find_result` 按首调用 args 查不到 → **副作用工具（send_message/express/delete_file）确定性重复执行**——正是 P1-2 要防的主场景。 | 🟢 `schema.rs:688` 唯一键；`llm_metrics.rs:221-226` ON CONFLICT 覆盖 args_json/result_json；`tool_loop.rs:775,611` attempt 恒 1；仓库测试只测查询层（`llm_metrics.rs:813-887`） | 唯一键加规范化参数哈希列（`UNIQUE(...,args_hash)`）；指纹统一用 `build_tool_fingerprint`/`stable_stringify`（顺带修 2.2 M9） |

### 2.2 MEDIUM（按域分组）

**API / 安全面**
| # | 位置 | 问题 |
|---|------|------|
| M1 | `approval.rs:190` + `tool_loop.rs:589` | 审批等待 `recv_timeout(120s)` 同步阻塞 tokio worker（全库无 spawn_blocking）；多审批并发可耗尽 worker 池，拖垮 HTTP/SSE/LLM（回环攻击者可借 H1 放大） |
| M2 | `approval.rs:192-198` | `allow_session` = 进程级全局放行（`HashSet` 永久驻留），无会话/用户维度、无过期——一次放行即长期免审高危工具 |
| M3 | `server.rs:203-220,246-265` | LAN+token 时浏览器客户端全废：鉴权在 OPTIONS 短路之前 → preflight 恒 403；`EventSource` 无法带 Authorization → LAN 上 `/events` SSE 不可用；静态 UI 页 GET 无法带 token 同样 403。另 `security.rs:5` 文档声称支持 `?token=` 未实现 |
| M4 | `capability/mod.rs:650-659` + `browser_tools.rs:1791-1858` | `browser_close clear_profile=true` 删除持久化登录态（cookie/登录）无需人工审批（Node 端动态提升 high，Rust 未接参数级 gate，注释自认） |
| M5 | `security.rs:244-254` | RateLimiter 键（来源 IP）永不过期 → 无界内存增长；IPv6 隐私地址轮换可绕限流 |
| M6 | `policy/mod.rs:294-326` | `network_decision` 潜伏绕过面（当前未接线，接线即成漏洞）：`http://user@169.254.169.254/`（userinfo）、`[::ffff:192.168.1.1]`/`[fd00::1]`（IPv6-mapped/ULA）、`http://2130706433/`（十进制 IP）均判 Allow |

**LLM / 工具循环**
| # | 位置 | 问题 |
|---|------|------|
| M7 | `tool_loop.rs:4,38-45` | 文档声称 `maxTotalCalls=30` 未实现——`ToolLoopLimits` 无该字段，最多 100 轮 × 每轮多工具 = 数百次执行无总闸 |
| M8 | `retry.rs:141-148` + `retry.rs:48` | `no_retry_429` 分支不可达：`is_transient_error(429)==false`，429 在 `!is_transient_error` 提前返回 → 限流信号在指标/周报丢失（无法区分 429） |
| M9 | `replay.rs:66-72` + `types.rs:349-375` | 重放键用非稳定 `Value::to_string()`（workspace 启用 `preserve_order`，按插入序；`3` vs `3.0` 亦不同）→ 同参重试键不匹配 → 防重放 miss；与熔断指纹 `stable_stringify` 不一致 |
| M10 | `tool_loop.rs:659-728` | 熔断后不终止循环：tripped 结果回喂后 `round+=1` 继续，模型重复同一工具则每轮空耗一次完整 LLM 调用（上限 100 轮） |
| M11 | `caller.rs:471-483` | 首轮流失败丢失已流出内容：`had_content=true` 时直接 `return Err`，已流出正文不在 `LlmCallResult.content`（与 Node 语义不一致） |

**DB / 数据**
| # | 位置 | 问题 |
|---|------|------|
| M12 | `llm/metrics.rs:305` + `llm_metrics.rs:918` | 周报/预算闸门时间口径混用：`day` 取本地日期（`chrono::Local`）前 10 位，窗口用 SQLite `date('now',?)`（UTC）→ +08:00 下每日 00:00–08:00 的调用跨日，7 天窗口边界漂移约 8 小时，预算闸门多/少算一天 |
| M13 | `llm_metrics.rs:354-361` | `prune_detail` 只清 `llm_calls`；`llm_tool_calls`（现 519 行）/`llm_context_sections`/`llm_turns` 无淘汰 → 随工具调用无限增长 |
| M14 | `schema.rs:94-145` | 启动期去重删除无事务、无备份、仅 warn 计数——老库一旦有重复键即静默删行不可回滚（当前真实库无重复，潜伏） |
| M15 | `memories.rs:35-101` + `connection.rs:52` | D4 残余：事务为 BEGIN DEFERRED 非 IMMEDIATE，跨进程（WAL 多写者）check-then-act 竞态仍在，靠部分唯一索引报错兜底 |

**工具层 / 执行**
| # | 位置 | 问题 |
|---|------|------|
| M16 | `web_tools.rs:590-596,669-677` | `fetch_url` 先 `resp.text()` 整读入内存（无大小上限，高速源可达 GB）再截断；`download_file` 流式写盘但无字节上限 → 可无限填满磁盘（无 Content-Length 预检、无压缩炸弹防护） |
| M17 | `tools/mod.rs:436-473` | exec_command 管道死锁（直连回落路径）：等待循环 `try_wait` 期间不排空 stdout/stderr，子进程写满管道缓冲（~4KB）即阻塞 → 产出 >4KB 的命令一律超时强杀、结果截断；且该路径**继承父进程完整环境**（无 env_clear）→ 向子进程泄露 `BAILONGMA_API_TOKEN`/`OPENAI_API_KEY`（sandbox 路径有 `env_remove`，`main.rs:141-148`） |
| M18 | `web_tools.rs:186-188,456-492` | `READ_CACHE` 进程级 HashMap 无条数/字节上限、无过期清理（SEARCH_CACHE 有 200 条上限，READ_CACHE 缺失）→ 长跑内存无界 |
| M19 | `browser_tools.rs:1941` + `tool_loop.rs:777` | schema 声明「fill/select 敏感值从审计日志脱敏」未实现：`record_tool_call` 把 `args.to_string()` 原样写入 `llm_tool_calls.args_json` → 表单密码/令牌明文落库持久化 |
| M20 | `web_tools.rs:1601-1614` | `check_url_ssrf` DNS 校验-再连接 TOCTOU：先 `ToSocketAddrs` 解析判私网，reqwest 连接时重新解析（DNS rebinding 可换绑），未固定已校验 IP |
| M21 | `validate.rs:88-143` | schema 声明的 maximum/minimum/maxLength/pattern/maxItems 全部未强制（只做 type/enum/items.type）；超长输入防线依赖各 impl 零散 clamp |

**运行时编排**
| # | 位置 | 问题 |
|---|------|------|
| M22 | `service.rs:412-460` | A4 残留：幂等 check-then-act 非原子，并发/在途重复 key 时 `create_turn` 撞唯一索引 → 「降级继续」执行（双扣费/双副作用）；应 `INSERT ... ON CONFLICT DO NOTHING` + 回查 |
| M23 | `turn/mod.rs:54-70` + `service.rs:461-471,529-533` | `can_transition_to` 生产从未调用（白名单未接线）；恢复代码可对 received 行直接 `mark_finished("failed")`——与矩阵禁止 `(received→failed)` 自相矛盾 |
| M24 | `turn/mod.rs:195-223` | 启动恢复只翻状态不续跑：恢复出的 running 行无驱动重放（无恢复队列/resume 入口），下次启动再被扫描 → 无限「翻状态」僵尸行；waiting_approval 挂起亦无人接管 |
| M25 | `intervention.rs:72-103` | 人工介入无控制面：`request_pause/resume/request_rescue` 仅测试调用，API 层无路由；暂停命中后循环直接 break，「暂停→续跑」「rescue→回滚重放」语义无落地点 |
| M26 | `matter.rs:359-369` | R3 残留：self_verified 分支 `record_signal` 在 completed 落库之后写，失败即「账已完成但返回 Err」；verify 全程（校验→transition→signal）三次独立写无事务 |
| M27 | `watchdog.rs:166-169` + `tools/mod.rs:589` | 僵尸 worker 复活：abort 无法抢占同步阻塞的 `executor.execute()`（审批 120s/命令轮询均为同步段），旧 worker 恢复后与重启的新循环**并发**跑完整唤醒 → 双广播/双 LLM 成本（与 H7 同源） |
| M28 | `tools/mod.rs:402-405` + `sandbox/main.rs:114-117` | `timeout_ms` 无上限（`as_u64` 直通）：LLM 可传 `86400000` 让命令跑一天并阻塞整轮消息/唤醒循环 |
| M29 | `sandbox/main.rs:511-546` | Unix `~` 家目录展开绕过黑名单（白名单模式下）：`cat ~/.ssh/id_rsa` argv[0]=cat 命中、无被拦元字符 → 读 root 外文件（Windows cmd 无此语义；受 H6 影响当前未启用） |
| M30 | `browser_tools.rs:760` + `1298-1304` | `profile.json` 从未写入 → `browser_sessions include_profiles` 恒为空；另 `808-830` 跨 root 重建会杀掉另一沙箱根的活动会话 |

### 2.3 LOW / INFO（合并列举）

| # | 位置 | 问题 |
|---|------|------|
| L1 | `injector_format.rs:686` | `<person>` 实体名未过 `sanitize_untrusted`（唯一 DB 内容裸拼接字段） |
| L2 | `messages.rs:464-499` | ToolResult 源（action_log/last_tool_result/recent_actions）无 SectionTag/转义直接拼入 runtime context——工具产物（网页/命令输出）是经典注入载体 |
| L3 | `temporal.rs:113-122` | `start_of_day` 用 `single().expect()`：DST 春季跳变午夜不存在/秋季歧义时 panic |
| L4 | `weather.rs:157-160` | 天气缓存无上限无淘汰（缓慢内存泄漏） |
| L5 | `matter.rs:413-433` | 时间比较字典序 + 格式耦合（`updated_at` `datetime('now')` vs `stale_before` ISO 参数）；`expire_stale`（四死法）生产零调用 |
| L6 | `scene.rs:102-105` + `store.rs:112-114` | hello 握手的 welcome.rev 与 snapshot.rev 分两次取锁，中间变更导致基线不一致 |
| L7 | `retrieval.rs:40-48` | `^TICK\s` 正则每次调用重编译（应 OnceLock） |
| L8 | `events.rs:135-157` | persist 回调在 `path` 锁内执行 DB 写（回调反入则死锁，潜伏） |
| L9 | `approval.rs:137-140` | id 熵用 RandomState（非密码学 RNG）——建议 getrandom/uuid |
| L10 | `security.rs:62-67` | token 比较长度不等早退（长度 orable）；建议固定长度哈希 |
| L11 | `security.rs:58` | IPv6 私网判定仅 `fe80:` 前缀，漏 fe80::/10 其余段（方向安全） |
| L12 | `policy/mod.rs:147-150` | audit_trail 环形裁剪 `drain(..excess)` 每次 O(n) memmove，建议 VecDeque |
| L13 | `approval.rs:16,288-315` | 「顺序无关」注释不成立：`set_global_on_request` 先于 `init_global` 时回调被整体替换静默丢失（当前生产顺序安全） |
| L14 | `desktop.rs:719` | 状态窗口事件 URL 硬编码 3721（与 DEFAULT_API_PORT 不一致） |
| L15 | `service.rs:707,712,963` | `wake_inflight.lock().unwrap()` 中毒连锁 panic（建议 into_inner） |
| L16 | `models.rs:170` | 搜索路径 NULL salience 硬读 i64 → InvalidColumnType（当前真实库无 NULL，潜伏） |
| L17 | `reminders.rs:33-34` / `conversations.rs:116-153` / `memories.rs:419-466` / `threads.rs:217` | `datetime()`/`strftime()` 表达式使索引失效（全表扫描；心跳每 60s 扫 conversations 1456 行）；threads 全表载入内存再过滤 |
| L18 | `brain_ui_events.rs:196` | `unchecked_transaction` 嵌套 BEGIN 报错后 best-effort 丢事件 |
| L19 | `llm_metrics.rs:156-157` | 乱序下 CallFailed 可覆盖已落库 round_limit（两终态均非 done，L6 小洞） |
| L20 | `evolution/mod.rs:87-88,119` | 回滚语义与注释不符：仅迭代开始前一次快照，第 N 轮失败会连先前已批准轮次一起回滚 |
| L21 | `retry.rs:212-214` | had_content 时记 `no_fallback_auth` 标签误导；`metrics.rs:76` 注释列 `no_retry_401` 代码从不记录 |
| L22 | `web_tools.rs:422` | READ 缓存键不含 max_chars/timeout_ms（不同参数命中同一截断结果） |
| L23 | `web_tools.rs:1799-1824` | `render_with_browser` 阻塞 read 使超时 kill 形同虚设（Chrome 卡死时工具挂死） |
| L24 | `sys_tools.rs:557-571` | `find_tool` 广告未注册工具（ui_set/speak/generate_image…），`is_ready` 对未知名返回 true → LLM 调用即「未知工具」 |
| L25 | `browser_tools.rs:808-830` | root 重建后旧 profile 锁自锁死（owner.pid 为本进程 → 永久 PROFILE_IN_USE） |
| L26 | `mod.rs:528-533` | `exec_via_sandbox` 的 read_line/wait 无超时（沙箱挂起则工具线程永久阻塞） |
| L27 | `web_tools.rs:87-89` | 同步 executor 内 `RT.block_on` 阻塞 tokio worker 最长 45s+（设计使然） |
| I1 | `logging.rs:134-142` | `unsafe fn winapi_is_console` 是死代码桩（不调 WinAPI），unsafe 多余 |
| I2 | `schema.rs:17-19` | 注释算术自相矛盾（27 vs 32 张表） |
| I3 | 库内时间格式两套口径 | matters 用 `datetime('now')`（无时区），turn_state/reminders/memories 用 RFC3339——跨表比较会错 |
| I4 | `matter_events`/`llm_tool_calls`/`llm_context_sections` 无 FOREIGN KEY | 删主行不级联，孤儿行累积 |
| I5 | 状态机生产不可达 | `matter.rs` 状态流转（start/submit/verify/cancel/expire）、`intervention` 全部仅测试引用；工具面只暴露 matter_create/matter_query |
| I6 | 释放包滞后 | `bailongma-rust-v0.1.0-release.zip`（含 bailongma-sandbox.exe）早于 S1/浏览器批等未提交修复——发布流程需重建 |

---

## 3. 依赖与构建审计

| 项 | 结论 |
|----|------|
| 依赖版本健康度 | 主要依赖均为当前线内较新版本：axum 0.8.9、reqwest 0.12.28、rusqlite 0.32.1（bundled）、tokio 1.53.1、rustls 0.23.43、scraper 0.27、wry 0.56/tao 0.36/tray-icon 0.24（desktop feature 可选）。 |
| 已知漏洞面 | h2 0.4.15 已修复 [RUSTSEC-2024-0332](https://rustsec.org/advisories/RUSTSEC-2024-0332)（CONTINUATION Flood，修复版 0.4.6+）✅；reqwest 栈相关 [RUSTSEC-2025-0134](https://rustsec.org/advisories/)（rustls-pemfile unmaintained）——本锁文件**无** rustls-pemfile，不受影响 ✅；axum-core [RUSTSEC-2022-0055](https://rustsec.org/advisories/RUSTSEC-2022-0055)（无默认请求体上限）——**适用且未缓解**：全 API 路由无 `DefaultBodyLimit`，`/message`/`/approval` 接受无界请求体（限流按来源 30/10s 仅缓频不限制单请求体大小）。 |
| 版本分裂（正常但需知悉） | tokio-tungstenite 0.26.2 + 0.29.0、tungstenite 0.26/0.29、dirs 5/6、rand 0.9/0.10、getrandom 0.2/0.3/0.4 并存（浏览器批引入 0.29 系）。 |
| 密钥卫生 | 源码/仓库无硬编码密钥（`audit_tmp` 仅为无密钥测试 fixture）；API 密钥经 config.json → config 表 → 环境变量三级注入；沙箱剥离敏感环境变量。 |
| 发布包 | 含 `bin/bailongma-sandbox.exe` → 生产默认走沙箱 exec 路径（但 `--allow` 未传，见 H6）；zip 相对工作区滞后。 |

---

## 4. 测试审计

| 项 | 结论 |
|----|------|
| 测试规模 | core/src 内 606 处 `#[test]`/`#[tokio::test]` + 集成测试（api_e2e / db_compat / escape_suite）；历史日志：342→352 passed 递增，1 ignored（db_compat 真实库条件跳过）。 |
| 覆盖亮点 | 安全回归（security_regression 15+）、审批协议 11、工具防重放故障注入（tool_loop 1552-1639）、watchdog 同步阻塞假死、escape_suite 逃逸向量、SSRF 表（web_tools 2600-2667）、L1-L6 修复均有测试锚定。 |
| **db_compat 32 vs 29 根因（已定位）** | `schema.rs initialize()` 对全部 32 张表**无条件 `CREATE TABLE IF NOT EXISTS`（无建表 guard）**，而 db_compat 只把 8 张观测表计入「允许新增」；历史失败重构：源库 21 张（缺 3 张遗留表 + 缺 8 张观测表）→ 迁移补 11 张而测试预期 8 张。**当前真实库 37 表齐全、增量=0，测试通过**（m3/m4 日志 1 passed）；但测试不变量脆弱：源库被旧版/部分备份替换即复现。建议改超集断言（迁移后 ⊇ 源表 ∪ 观测表）。 |
| **无近期全量测试** | 现存 cargo_test*.log 全部为 2026-08-10（早于 Phase 1/2 审计与当前未提交改动）；最近提交自称「24 测试全绿」为提交时点验证。**未提交改动（含 browser_tools.rs 约 2300 行新代码）无整仓回归运行记录**——建议提交前跑 `cargo test --workspace` + `cargo clippy -- -D warnings`。 |

---

## 5. 亮点（正面确认）

- **S1 修复质量高**：审批门从测试专用变为「装配即接线」，`init_global` 可变槽 + fail-closed 隔离门消除 OnceLock 顺序依赖；`execute()` 统一 guard 覆盖全部 needs_approval 工具，且有生产接线单测。
- **A2 交付后消费闭环**：`due_wakeup` 查询态 + `wake_inflight` 串行化 + LLM 失败/空回复原文兜底，提醒在广播/LLM 失败下只延迟不丢失；预算闸门先有账再设闸。
- **直连 HTTP 的 SSRF 防线完整**：`check_url_ssrf` 协议/凭据/本机/私网/云元数据全覆盖（含 CGNAT 100.64/10、198.18/15、IPv6 v4-mapped 递归），`redirect_policy` 每跳 `Attempt::error` 中止，`allow_lan` 默认 false。
- **D1-D3 三方证据**：datetime 归一比较 / 去重后建唯一索引 / FTS flag 增量重建，均有代码+测试+真实库实测。
- **sandbox 工程化**：父目录 canonicalize + 真实落点返回（顺带消 B4）、分读线程 + mpsc 有界收尾、超长 RPC 行丢弃协议继续、进程组强杀。
- **指标账本严谨**：UPSERT「成功覆盖错误、错误不覆盖成功」内存+DB 双层守卫、attempt MAX、flusher 零 panic 路径、pending 有界。
- **测试纪律**：安全原语（常量时间比较、fail-closed、防重放、SSRF 表）均有独立回归测试；无 TODO/FIXME 堆积（全仓仅 2 处，均非阻塞）。

---

## 6. 修复路线图（按风险/成本比排序）

**第一批（安全，建议 C 档逐项确认后修复）**
1. H1 `Origin: null` 拒绝/强制 token + CORS 不反射 null（含 `/scene` WS 与 `/approval` 关联鉴权）。
2. H3 + H4 浏览器执行路径接入 SSRF 校验（`render=browser` 前置 `check_url_ssrf`；导航后复检终态 URL）。
3. H2 core `resolve_under_root` 移植父目录 canonicalize（与 sandbox 收敛共享模块）。
4. H6 生产注入 `--allow` 最小白名单或黑名单无条件生效。

**第二批（正确性/可用性）**
5. H7 唤醒循环：`run_wakeup_turn` 期间心跳 + 重启回调清空 `wake_inflight`。
6. H8 + M9 台账唯一键加参数哈希列（顺手统一 `stable_stringify`）。
7. M22 A4 幂等 `INSERT ... ON CONFLICT DO NOTHING` + 回查；M12 预算窗口统一 UTC。
8. M16/M17 响应体上限 + exec 管道排空 + 直连路径 `env_clear`。

**第三批（防御纵深/债务）**
9. H5 PolicyEngine 接线（`is_path_denied` + 输出脱敏）；M1 spawn_blocking；M2 allow_session 收敛；M13 观测表淘汰；M19 fill 值掩码。
10. M23/M24 turn 状态机接线与恢复续跑；M25 介入控制面；M26 matter 事务化。
11. LOW 批（DST panic、正则重编译、缓存上限、索引失效、注释/文档对齐）。

> 与 REVIEW_PROCESS 对应：第 1、6、9 批涉及安全策略/数据删除语义，属 C 档逐项停；其余可按 B 档波次推进，每波附 diff stat + 测试结果。

---

## 7. 一句话总结

Phase 2 的 29 项遗留中 23 项确认真实修复、6 项残留（L1 写侧/D4/A4/R3/B1 core 面/B5 生产未启用）直接构成 H2/H6/H8；本轮新增高危集中在**未提交的新代码**（浏览器执行路径 SSRF 全绕过）与**修复的边界残留**（Origin null 放行、core 文件工具 junction 逃逸、watchdog 重启后唤醒停摆、PolicyEngine 零接线）——共 8 项 HIGH、30 项 MEDIUM、27 项 LOW/INFO，修复优先级见路线图；工作区未提交改动本身（除 browser_tools.rs 新代码外）经逐 hunk 核对无回归，但建议提交前补一次整仓全量回归。

---

*本报告为只读审计交付：未编译、未改码；🟡 锚点行号以子代理报告为准，修复前建议复核。*
