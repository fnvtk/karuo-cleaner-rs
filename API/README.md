# 卡若清理本机 API

本接口供本机 Codex 或脚本调用实际 Rust 引擎，GUI 必须已运行。仅监听当前用户的 Unix socket，文件权限 `0600`，连接时核对同一 UID；不监听网络端口。每个请求是一行 JSON。以下命令中的 ID、目录、计划编号都应替换为真实回读值，示例不会自动执行。

```sh
CLEANER='/Applications/卡若清理.app/Contents/MacOS/KaruoCleaner'
"$CLEANER" --api '{"cmd":"capabilities"}'
"$CLEANER" --api '{"cmd":"ready"}'
```

`capabilities` 可在 GUI 未启动时读取。其余接口需要已运行的应用。也可使用标准输入，避免复杂 JSON 的 shell 转义：

```sh
"$CLEANER" --api - <<'JSON'
{"cmd":"session_get","module":"junk"}
JSON
```

## 扫描、结果保留和分页

模块为 `smart`、`junk`、`uninstall`、`large`、`space`。空间透镜使用专用 `space_start`，其他模块使用 `scan`。`optimize` 返回应用运行状态，不是文件扫描模块。

```sh
"$CLEANER" --api '{"cmd":"scan","module":"junk"}'
"$CLEANER" --api '{"cmd":"session_get","module":"junk"}'
"$CLEANER" --api '{"cmd":"scan_page","module":"junk","category":"cache","query":"","page":0}'
"$CLEANER" --api '{"cmd":"cancel","module":"junk"}'
```

`scan`、`space_start`、`cancel` 返回 `queued:true` 只表示入队，不代表成功或完成。必须用 `session_get` 回读 `event:session`，检查外层 `scanning`、`data.done`、`data.notes`、`data.truncated` 和实际条目。取消可能等待正在进行的系统读取返回；不要把已发送取消当成线程已经结束。

`session_get` 不重新扫描。模块之间互不取消；同一模块重新扫描会替换旧任务和旧结果。切换界面标签不会取消后台任务。普通候选结果完成后保存本机快照，重启后可恢复；空间透镜的完整目录树只在当前应用进程内保留，不跨重启保存。

分页页码从 `0` 开始，每页最多 50 条，仅限制传输和显示，不截断扫描文件数量。普通扫描返回 `entries`、`total`、`filtered_total`、`page`、`page_count`、`total_bytes`、`filtered_bytes`、`scanned`、`skipped`、`notes`、`done`、`running`。分类为 `all/cache/log/application/file`。`query` 只匹配本机已扫描名称和路径，不读取文件内容。

### 任意目录与整盘

先通过只读 `root_set` 设置绝对目录，无需文件选择框；它不会单独授予删除整个目录的权限。

```sh
"$CLEANER" --api '{"cmd":"root_set","module":"large","path":"/Users/karuo/Downloads"}'
"$CLEANER" --api '{"cmd":"scan","module":"large","min_mb":100,"old_only":false}'
"$CLEANER" --api '{"cmd":"session_get","module":"large"}'
"$CLEANER" --api '{"cmd":"volumes"}'
```

整盘扫描可使用 `volumes` 返回的真实 `id`：

```json
{"cmd":"space_start","volume":"从 volumes 读取的 id"}
```

也可设置空间目录再扫描、浏览：

```sh
"$CLEANER" --api '{"cmd":"root_set","module":"space","path":"/System/Volumes/Data"}'
"$CLEANER" --api '{"cmd":"space_start","path":"/System/Volumes/Data"}'
"$CLEANER" --api '{"cmd":"session_get","module":"space"}'
"$CLEANER" --api '{"cmd":"space_browse","path":"/System/Volumes/Data/Users","page":0,"query":"","show_hidden":false}'
```

`space_browse` 仅接受当前树中已经索引的目录。空间分页返回当前目录的页；换目录时传 `page:0`。空间透镜只分析占用，不直接删除目录。

## 清理必须预览再执行

写操作 `move/restore/purge/terminate/startup_set/uninstall_apply` 不能直接通过 API 执行。先发 `preview`，审阅请求，再使用返回的 `preview_id` 发 `apply`。凭证 5 分钟有效、只能使用一次，应用重启后失效。不要自动重试失败的写操作，先回读真实状态。

```json
{"cmd":"preview","action":{"cmd":"move","module":"junk","ids":["从 entries 读取的真实 id"]}}
```

```json
{"cmd":"apply","preview_id":"上一步返回值","confirmed":true}
```

按当前分类和搜索结果全选，使用 `selection`，范围是后端全部匹配候选，而非当前 50 条：

```json
{"cmd":"preview","action":{"cmd":"move","module":"junk","selection":{"category":"cache","query":""}}}
```

`preview` 展示待执行请求，不表示完整磁盘安全检查已经通过。执行时仍检查候选身份、路径、保护名单、占用和权限，只将允许项移入废纸篓。AI 建议不替代这些检查，也不自动批准删除。处理后回读 `history` 与相关模块 `session_get`，核对 `moved` 的成功条目和错误；移入废纸篓通常不会立即释放空间。

## AI 配置与辅助分析

配置不返回完整密钥。默认仅发送匿名文件类型、大小与时间的分类汇总，不发送文件名、路径或内容。

```sh
"$CLEANER" --api '{"cmd":"ai_get"}'
"$CLEANER" --api '{"cmd":"ai_models"}'
"$CLEANER" --api '{"cmd":"ai_analyze","module":"junk","category":"cache","query":""}'
"$CLEANER" --api '{"cmd":"ai_analyze","module":"optimize"}'
```

`ai_save` 请求字段为 `base_url`、`model`、`api_key`。不要把实际密钥放入文档、聊天、shell 历史或日志。通过本机私有输入生成 JSON，再用 `--api -` 发送；`api_key` 留空会保留已有密钥；首次配置必须通过私有输入提供实际值。保存后再读 `ai_get`，不假设配置成功。模型服务错误或回退说明不是 AI 分析成功；应核对响应来源和错误字段。

## 保护名单

```json
{"cmd":"ignore_get"}
{"cmd":"ignore_add","id":"从扫描候选读取的真实 id"}
{"cmd":"ignore_remove","path":"从保护名单读取的真实路径"}
```

新增只能来自后台已批准候选。内置系统和个人资料保护规则不会被解除用户保护名单的动作覆盖。保护名单修改后使用 `ignore_get` 回读。

## 定时任务

```json
{"cmd":"schedule_get"}
{"cmd":"schedule_save","enabled":false,"frequency":"weekly","hour":10,"minute":0,"mode":"scan"}
```

支持 `daily/weekly`，`mode` 为 `scan/trash`，默认关闭。开启定时 `trash` 仍要求原生确认；仅处理保守规则内的旧缓存和日志。单次自动清理 50 项、128 MB 是删除动作安全阈值，不是扫描数量上限，仍保留。保存后以 `schedule_get` 回读实际配置和系统任务状态。

## 历史、恢复与永久删除

```json
{"cmd":"history"}
{"cmd":"preview","action":{"cmd":"restore","ids":["history 返回的记录 id"]}}
{"cmd":"preview","action":{"cmd":"purge","ids":["history 返回的记录 id"]}}
```

之后按统一 `apply` 执行。`purge` 即使拿到 API 预览凭证，仍要求本机原生永久删除确认，不能静默绕过。仅作用于本应用历史记录，不清空其他废纸篓内容。`restore/purge` 可能先返回 `message`，之后才产生历史更新；必须再次请求 `history` 核对状态，并必要时检查真实路径。

## 运行应用与登录项

```json
{"cmd":"optimize"}
{"cmd":"preview","action":{"cmd":"terminate","pids":[12345]}}
{"cmd":"startup_get"}
{"cmd":"preview","action":{"cmd":"startup_set","ids":["startup 返回的真实 id"],"enabled":false}}
{"cmd":"login_settings"}
```

`terminate` 只发送正常退出请求，应用可能弹出保存提示，返回请求已发送不代表已退出；用 `optimize` 再读进程。登录项先检查 `manageable/reason`，只更改可管理的当前用户项，不删除 plist、不修改系统 daemon。现代系统登录项如不能安全管理，会要求转到系统设置。修改后用 `startup_get` 回读。

## 深度卸载计划

先扫描 `uninstall`，一次选一个应用建立计划；显式审阅关联资料，不把应用资料误当成普通缓存。

```json
{"cmd":"scan","module":"uninstall"}
{"cmd":"session_get","module":"uninstall"}
{"cmd":"uninstall_plan","ids":["应用候选真实 id"]}
{"cmd":"uninstall_analyze","plan_id":"返回的 plan_id"}
{"cmd":"preview","action":{"cmd":"uninstall_apply","plan_id":"同一 plan_id","ids":["计划中明确勾选的真实条目 id"]}}
```

计划 AI 审阅成功后，才允许精确条目进入卸载处理；不能只凭模型建议任意删除 Library。通过统一 `apply` 后，再读 `history` 和 `session_get`。偏好设置和保存状态可能含个人配置，应审阅后单独选择。

## 真实完成判定

API 可能先收到 `event:message`，这不是最终 `history`、`startup` 或扫描状态的替代。`queued:true`、成功建立预览、发送正常退出、得到 AI 建议，都不能独立证明完成。调用者应在对应业务状态回读后报告成功；超时先查询当前状态，避免重复清理、恢复或卸载。


## 对话、空间清理与撤回（build 11）

- `ai_chat {module,text,ids?,category?,query?,history?}` 返回 `ai_chat {reply,proposals,source,usage,error?}`。匿名角色最多40项示例加全部统计仅用于模型上下文，不是扫描截断。对话中的已知路径/选中名/密钥会隐藏。`history`为role/content数组。
- `ai_proposal_plan {proposal_id}` 返回cleanup_plan / uninstall_plan / ai_schedule_proposal；定时仅返回config草案，未保存或执行。
- `space_cleanup_plan {ids:[当前目录索引path]}`，或 `{selection:{query,show_hidden}}` 选择当前目录匹配项（含目录已索引普通文件）；不会整体移动目录。
- `cleanup_plan_page {plan_id,page}` 每页50项，total/page_count展示全部；`cleanup_apply {plan_id,ids}` 或all:true。必须外层preview/apply确认。
- `cleanup_analyze {plan_id}` 可作额外汇总建议，不能扩大计划。
- `undo_last` API同样preview/apply；只恢复本次启动最近成功批次，返回undo/restored/errors。重启后的旧记录使用history/restore。保护/身份/占用仍有效。
- `rules_get` 返回只读扫描和清理规则。

示例：
```json
{"cmd":"ai_chat","module":"space","text":"帮我检查哪些可重建文件适合清理，先给清单"}
```
```json
{"cmd":"ai_proposal_plan","proposal_id":"使用真实返回的建议编号"}
```
```json
{"cmd":"preview","action":{"cmd":"cleanup_apply","plan_id":"真实plan_id","all":true}}
```
```json
{"cmd":"apply","preview_id":"真实一次性预览编号","confirmed":true}
```

返回后仍需session_get/history和文件状态回读，queued或模型建议不代表清理完成。

## 清理管家（build 12）

`guardian_get` 返回偏好、真实定时任务状态、最近报告、已恢复项目数量及网站绑定状态。当前绑定为 pending，不代表已接入网站。`guardian_save` 仅保存保留习惯，不启用定时任务或授予删除权限。

```json
{"cmd":"guardian_get"}
{"cmd":"guardian_save","preferences":{"cache_days":30,"log_days":90,"clean_cache":true,"clean_logs":true}}
```

缓存保留14–3650天、日志保留90–3650天；默认30/90天。自动候选还必须通过原白名单、保护、占用与身份检查。恢复过的原路径及后代不再进入自动清理；手工清理仍按明确选择处理。读取偏好或恢复历史失败时暂停自动移动并保存报告，执行过程中再次读取偏好。

无窗口检查可使用 `--guardian-check`，只读状态；定时执行仍由 `--scheduled-run` 入口负责。网站Agent只能在正式凭证下登记并回读，不把本机管家名称当作网站绑定。

### 扫描结果中的保护提示（build16）

`session_get` / `scan_page` 的每项增加 `protected` 和 `protection_reason`，整份筛选增加 `selectable_total` 与 `protected_total`。受保护项目保持可见，但客户端应禁选并显示原因。`move.selection` 的全选在后台再次排除当前保护名单和内建路径保护；不要只以当前页判断可处理数量。

这是展示与选择信息，不是清理授权。文件在扫描后可能改变，真正移动前仍以服务端身份、占用和保护校验为准。保护名单暂时无法读取时，`selectable_total=0`，应等待读取恢复，不得当作空名单。

### 空间结果恢复（build17）

`space_start`启动的索引在结束或主动停止后保存；`session_get(module="space")`可回读`restored`、`saved_at`（秒）、`cancelled`、`truncated`、`snapshot_notice`、`source_volume`、`page`、`query`和`show_hidden`。`space_browse`更新浏览位置。`snapshot_notice`提示正在保存、保存失败或新扫描未完成，不应把`done=true`等同于已完整保存或已完整读取整盘。

重启后恢复旧索引不意味着文件仍存在或可以删除；空间清理仍走`space_cleanup_plan`和preview/apply，重新检查真实文件身份与保护规则。索引尚在加载时其他模块仍可用，启动新扫描会使旧恢复任务失效。
