# 跨平台 YT-DLP 桥接下载器技术设计 V1

## 1. 文档目的

本文档基于 [design.md](e:\YoutuDlp\design.md) 的已确认规格，给出可直接进入实现阶段的技术设计。

本文档重点覆盖：

- Rust Bridge 模块划分
- 本地状态目录与 JSON 文件结构
- HTTP API 设计
- SSE 事件模型
- 调度、恢复、下载库索引的实现边界
- sidecar 工具发现与调用策略

本文档不写死具体 Rust Web 框架与第三方库，但会固定内部模块边界和行为约束。

---

## 2. 总体实现结构

V1 采用单进程 Bridge 架构：

- 一个 Rust 可执行文件承载 HTTP 服务、任务调度、状态持久化、下载库管理与 sidecar 调用
- 一个内嵌的单页 HTML Web App 由 Bridge 直接提供
- 浏览器通过 HTTP API 发起操作，通过 SSE 接收状态与日志增量
- `yt-dlp` 默认由 Bridge 从可执行文件同目录发现并调用
- `ffmpeg` 与 `ffprobe` 建议采用相同发现规则

运行时分为 4 类资源：

- 程序目录中的可执行文件与 sidecar 工具
- 用户数据目录中的 JSON 状态文件
- 下载根目录中的下载内容与 sidecar 文件
- 浏览器中的单页 Web App

---

## 3. 运行目录模型

### 3.1 程序目录

程序目录指 Bridge 可执行文件所在目录。

预期内容：

- `YoutuDlpBridge(.exe)`
- `yt-dlp(.exe)`
- `ffmpeg(.exe)` 可选
- `ffprobe(.exe)` 可选

程序目录默认只读看待，不写入运行状态。

### 3.2 用户数据目录

用户数据目录用于保存所有运行状态。

建议逻辑名称：

- Windows: `%APPDATA%/YoutuDlpBridge`
- macOS: `~/Library/Application Support/YoutuDlpBridge`
- Linux: `$XDG_CONFIG_HOME/YoutuDlpBridge` 或 `~/.config/YoutuDlpBridge`

建议目录结构：

```text
state/
  settings.json
  queue.json
  tasks/
    task_<id>.json
  playlists/
    playlist_<id>.json
  attempts/
    <taskId>/
      001.json
      002.json
  library/
    index.json
    scan-state.json
  runtime/
    session.json
    locks.json
  logs/
    bridge.log
```

### 3.3 下载根目录

下载根目录仅承载下载结果与 sidecar 文件，不承载 Bridge 的运行状态。

下载根目录下允许存在：

- 媒体文件
- 字幕文件
- 缩略图文件
- `.info.json`
- `.part` 与片段文件

下载根目录下不允许创建 Bridge 专用隐藏状态目录作为唯一真实来源。

---

## 4. Rust Bridge 模块划分

建议模块结构如下：

```text
src/
  main.rs
  app/
    mod.rs
    state.rs
    startup.rs
    shutdown.rs
  api/
    mod.rs
    routes.rs
    dto.rs
    errors.rs
    sse.rs
  settings/
    mod.rs
    model.rs
    service.rs
  tasks/
    mod.rs
    model.rs
    service.rs
    scheduler.rs
    transitions.rs
    recovery.rs
    duplicate.rs
  playlists/
    mod.rs
    model.rs
    service.rs
  downloads/
    mod.rs
    yt_dlp.rs
    command_builder.rs
    progress_parser.rs
    process_registry.rs
    stop.rs
  library/
    mod.rs
    model.rs
    scanner.rs
    tree.rs
    move_ops.rs
    delete_ops.rs
    open_ops.rs
  storage/
    mod.rs
    json_store.rs
    atomic_write.rs
    paths.rs
  tools/
    mod.rs
    locator.rs
    versions.rs
  security/
    mod.rs
    paths.rs
    names.rs
    urls.rs
    cookies.rs
  events/
    mod.rs
    bus.rs
    types.rs
  web/
    mod.rs
    assets.rs
```

### 4.1 app

职责：

- 进程级初始化
- 启动顺序控制
- 全局应用状态持有
- 优雅退出

`AppState` 建议统一持有：

- 设置服务
- 任务服务
- 调度器句柄
- 下载库服务
- 存储服务
- 事件总线
- 运行中进程注册表

### 4.2 api

职责：

- HTTP 路由注册
- 请求体与响应体编解码
- 参数校验错误映射
- SSE 长连接输出

要求：

- API 层不直接操作文件系统
- API 层不直接拼装 `yt-dlp` 命令
- API 层仅调用应用服务并返回 DTO

### 4.3 tasks

职责：

- 任务状态机
- 队列与调度
- 重复检测
- 恢复逻辑
- 任务级增删改查

约束：

- 单视频任务和播放列表子任务是可调度单元
- 播放列表父任务只做聚合视图
- 状态流转必须集中在 `transitions.rs`，避免散落

### 4.4 downloads

职责：

- 构建 `yt-dlp` 命令参数
- 启动和监控子进程
- 解析 stdout/stderr 中的进度与日志
- 处理 Stop、Resume、Restart 对应的运行逻辑

约束：

- 所有 `yt-dlp` 调用都必须经过命令构建器
- 所有运行中的进程都必须登记到 `process_registry`
- 停止逻辑统一从 `stop.rs` 走，避免多处 kill

### 4.5 library

职责：

- 扫描下载根目录
- 维护下载库索引缓存
- 构建树状结构
- 执行移动、删除、创建目录、打开文件/目录

约束：

- 树结构是查询时视图，不是持久化主结构
- 索引缓存损坏时允许全量重建

### 4.6 storage

职责：

- JSON 文件读写
- 原子写入
- 路径定位
- 锁与并发写控制

约束：

- 业务模块不得直接 `std::fs::write` 状态文件
- 所有状态写入必须走 `atomic_write`

### 4.7 tools

职责：

- 查找 `yt-dlp`、`ffmpeg`、`ffprobe`
- 读取版本信息
- 启动时输出工具可用性诊断

### 4.8 security

职责：

- URL 白名单校验
- 路径越界校验
- 目录名合法性校验
- Cookie 文件路径与格式校验

所有文件系统入口都必须复用这里的校验函数。

### 4.9 events

职责：

- 进程内事件总线
- SSE 广播
- 事件类型定义

要求：

- API 不直接监听下载进度来源
- 下载、调度、索引变化统一先进入事件总线，再由 SSE 输出

---

## 5. 核心数据模型

### 5.1 Task

`Task` 表示一个可执行下载单元，包含单视频任务与播放列表子任务。

建议字段：

```json
{
  "id": "task_01H...",
  "kind": "video",
  "parentPlaylistId": null,
  "state": "queued",
  "sourceUrl": "https://...",
  "extractor": "youtube",
  "videoId": "abc123",
  "title": "Example Title",
  "uploader": "Example Uploader",
  "durationSeconds": 1234,
  "thumbnailUrl": "https://...",
  "estimatedSizeBytes": 123456789,
  "targetSubdir": "Music",
  "outputDirRelative": "Music",
  "outputTemplate": "%(title)s [%(id)s].%(ext)s",
  "downloadMode": "audio",
  "quality": "best",
  "container": "mp3",
  "subtitleOptions": {
    "enabled": false,
    "languages": []
  },
  "cookieImport": {
    "enabled": false,
    "fileName": null
  },
  "advancedOptions": {
    "rawArgs": [],
    "rawFormatCode": null
  },
  "duplicateKey": {
    "extractor": "youtube",
    "videoId": "abc123"
  },
  "outputFiles": [],
  "createdAt": "2026-03-29T10:00:00Z",
  "updatedAt": "2026-03-29T10:00:00Z",
  "lastError": null,
  "attemptCount": 0,
  "lastAttemptId": null,
  "progress": {
    "percent": 0,
    "downloadedBytes": 0,
    "totalBytes": null,
    "speedBytesPerSec": null,
    "etaSeconds": null
  }
}
```

### 5.2 Playlist

`Playlist` 表示播放列表父任务。

建议字段：

```json
{
  "id": "playlist_01H...",
  "sourceUrl": "https://...",
  "title": "Playlist Name",
  "extractor": "youtube:tab",
  "targetSubdir": "Playlists",
  "outputDirRelative": "Playlists/Playlist Name",
  "childTaskIds": ["task_a", "task_b"],
  "totalCount": 80,
  "completedCount": 12,
  "failedCount": 1,
  "activeChildTaskId": "task_m",
  "createdAt": "2026-03-29T10:00:00Z",
  "updatedAt": "2026-03-29T10:00:00Z"
}
```

### 5.3 Attempt

`Attempt` 表示一次实际下载尝试。

建议字段：

```json
{
  "id": "attempt_01",
  "taskId": "task_01H...",
  "attemptNumber": 1,
  "mode": "fresh",
  "startedAt": "2026-03-29T10:05:00Z",
  "endedAt": null,
  "exitCode": null,
  "commandSummary": {
    "downloadMode": "audio",
    "quality": "best",
    "container": "mp3",
    "rawArgsApplied": []
  },
  "lastProgress": {
    "percent": 17.4,
    "downloadedBytes": 12345678,
    "totalBytes": 77777777,
    "speedBytesPerSec": 1048576,
    "etaSeconds": 61
  },
  "errorSummary": null,
  "logTail": [
    "[download]   1.7% of 74.12MiB at 1.01MiB/s ETA 01:11"
  ]
}
```

`mode` 建议值：

- `fresh`
- `resume`
- `restart`

### 5.4 Queue

`queue.json` 只保存轻量顺序信息，不重复保存完整任务体。

建议结构：

```json
{
  "taskOrder": ["task_a", "task_b", "task_c"],
  "updatedAt": "2026-03-29T10:00:00Z"
}
```

### 5.5 Settings

建议结构：

```json
{
  "downloadRoot": "D:/Downloads",
  "maxConcurrentDownloads": 3,
  "openBrowserOnFirstLaunch": true,
  "openBrowserOnLaterLaunch": true,
  "incompleteTaskRecovery": "manual",
  "defaultTargetSubdir": "",
  "showAdvancedByDefault": false,
  "defaultDownloadMode": "video",
  "defaultContainer": "mp4",
  "defaultQuality": "1080p",
  "toolOverrides": {
    "ytDlpPath": null,
    "ffmpegPath": null,
    "ffprobePath": null
  }
}
```

---

## 6. JSON 文件职责边界

### 6.1 settings.json

职责：

- 保存全局设置
- 首次启动标记
- 工具路径覆盖

### 6.2 queue.json

职责：

- 保存任务显示顺序
- 保存队列更新时间

不保存：

- 完整任务详情
- 下载日志

### 6.3 tasks/*.json

职责：

- 任务主记录
- 状态、参数、输出文件、最近错误、最近进度

### 6.4 playlists/*.json

职责：

- 父任务聚合数据
- 子任务引用

### 6.5 attempts/*/*.json

职责：

- 每次执行的摘要
- 最近日志尾部
- 退出码与错误原因

V1 不要求持久化全量历史日志，只保留适合 UI 展示的窗口化日志。

### 6.6 library/index.json

职责：

- 下载库平铺索引缓存
- 目录与文件元信息缓存
- 从 `.info.json` 提取的重复检测辅助信息

### 6.7 runtime/session.json

职责：

- 上次运行中的活跃任务快照
- 启动恢复所需的临时状态

应用正常关闭时应更新为干净状态，异常退出后可用于恢复判断。

---

## 7. 状态写入策略

统一写入步骤：

1. 序列化到内存缓冲区
2. 写入目标文件同目录下的 `.tmp` 文件
3. flush
4. 原子 rename 覆盖正式文件

附加约束：

- 单个任务状态变化仅更新对应 `tasks/<id>.json`
- 队列顺序变化仅更新 `queue.json`
- 尝试记录变化仅更新对应 `attempts/.../*.json`
- 下载库变化优先更新 `library/index.json`

为降低损坏面，禁止“每次状态变化重写整棵 state 目录”。

---

## 8. 启动流程

### 8.1 启动顺序

建议顺序：

1. 定位程序目录
2. 定位用户数据目录
3. 创建缺失的状态目录
4. 发现 `yt-dlp`、`ffmpeg`、`ffprobe`
5. 加载 `settings.json`
6. 加载 `queue.json`、`tasks`、`playlists`
7. 读取 `runtime/session.json` 并执行恢复修正
8. 建立事件总线
9. 启动 HTTP 服务与 SSE
10. 根据设置决定是否自动打开浏览器

### 8.2 首次启动

首次启动判定条件：

- `settings.json` 不存在，或存在但缺少已初始化标记

首次启动行为：

- 生成默认设置文件
- 默认打开浏览器
- 要求用户确认下载根目录

### 8.3 恢复修正规则

启动时对任务状态做如下修正：

- `queued` 保持 `queued`
- `paused` 保持 `paused`
- `downloading` 转为 `paused`
- `stopping` 转为 `paused`
- `submitted` 或 `resolving` 转为 `failed` 或重新回到 `submitted`，具体由实现选择，但必须一致

推荐做法：

- `submitted` 与 `resolving` 转为 `failed`
- 最近错误写为“应用在解析阶段异常退出”

### 8.4 session.json 建议结构

```json
{
  "startedAt": "2026-03-29T10:00:00Z",
  "cleanShutdown": false,
  "activeTaskIds": ["task_a", "task_b"],
  "lastKnownState": {
    "task_a": "downloading",
    "task_b": "queued"
  }
}
```

---

## 9. 调度器设计

### 9.1 调度循环

调度器维护一个持续运行的循环或触发式检查器，在以下情况重新评估调度：

- 新任务入队
- 活跃任务结束
- 用户修改并发数
- 任务从暂停恢复
- 播放列表子任务生成完毕

### 9.2 调度规则

仅满足以下条件的任务可被启动：

- 状态为 `queued`
- 不属于被删除或取消对象
- 当前活动下载数小于并发上限

选择顺序：

- 按 `queue.json` 中的 FIFO 顺序

### 9.3 活跃任务定义

计入并发槽位的状态：

- `downloading`
- `stopping`

不计入并发槽位的状态：

- `queued`
- `paused`
- `failed`
- `completed`
- `canceled`

### 9.4 停止后的调度

当任务进入 `stopping` 时，是否立即释放并发槽位需要固定规则。

建议规则：

- 仅在进程真正退出后释放槽位

这样可以避免 UI 显示与真实资源占用不一致。

---

## 10. 下载执行设计

### 10.1 Resolve

解析操作与正式下载分离。

`resolve` 阶段只负责：

- 拉取元数据
- 判断是否为播放列表
- 提取可展示的格式与大小信息
- 提取重复检测所需键值

`resolve` 阶段不负责：

- 正式下载
- 占用下载并发槽位

### 10.2 Download

正式下载阶段：

- 根据任务参数构建 `yt-dlp` 命令
- 进入 `downloading`
- 解析进度输出
- 持续更新任务与 attempt
- 完成后写回输出文件清单

### 10.3 Resume

继续下载不是恢复进程内存态，而是：

- 基于已有任务参数重新构建命令
- 保留部分文件
- 以 `resume` 模式创建新 attempt

### 10.4 Restart

Restart 流程：

1. 校验任务当前允许重启
2. 删除该任务已知的部分文件
3. 重置任务进度与最近错误
4. 创建 `restart` 模式 attempt
5. 重新排队或直接启动

### 10.5 Stop

建议停止流程：

1. 任务状态转为 `stopping`
2. 向子进程发送优雅终止信号
3. 等待固定超时
4. 若未退出则强制终止
5. 写入最终 attempt 状态
6. 任务转为 `paused`

---

## 11. 重复检测设计

### 11.1 主键

主判定键固定为：

- `extractor + videoId`

### 11.2 参考来源

参考来源包括：

- 已存在任务
- `library/index.json`
- 下载根目录中扫描得到的 `.info.json`

### 11.3 检测结果结构

建议返回：

```json
{
  "status": "duplicate-found",
  "key": {
    "extractor": "youtube",
    "videoId": "abc123"
  },
  "matches": [
    {
      "source": "library",
      "path": "Music/Example.mp3",
      "taskId": null,
      "container": "mp3",
      "quality": "best"
    }
  ],
  "defaultAction": "skip",
  "allowedActions": ["skip", "add_anyway", "open_existing"]
}
```

---

## 12. 下载库索引设计

### 12.1 索引目标

下载库索引保存平铺项，不保存树结构。

每个索引项建议包括：

- 相对路径
- 类型：文件或目录
- 文件大小
- 修改时间
- 是否隐藏
- 关联的提取器与视频 ID（如可从 `.info.json` 推断）

### 12.2 树构建

API 返回树时，根据平铺索引即时构造树节点。

排序规则：

- 文件夹在前
- 文件在后
- 同层按名称排序

### 12.3 刷新策略

刷新来源分为两类：

- 下载任务引发的自动刷新
- 用户主动点击刷新触发的全量扫描

V1 不要求强实时文件系统监听。

---

## 13. HTTP API 设计

### 13.1 通用约定

- 所有 API 采用 JSON 请求与 JSON 响应
- 所有路径都位于 `/api`
- 时间统一用 ISO 8601 UTC 字符串
- 错误响应统一结构

建议错误结构：

```json
{
  "error": {
    "code": "INVALID_TARGET_PATH",
    "message": "Target path is outside download root.",
    "details": null
  }
}
```

### 13.2 设置相关

- `GET /api/settings`
- `PUT /api/settings`

`PUT /api/settings` 可修改：

- 下载根目录
- 最大并发数
- 浏览器自动打开策略
- 恢复策略
- 默认目标子目录
- 高级模式偏好
- 工具路径覆盖

### 13.3 解析相关

- `POST /api/resolve`

请求示例：

```json
{
  "url": "https://...",
  "downloadMode": "video",
  "quality": "1080p",
  "container": "mp4",
  "targetSubdir": "Video",
  "advanced": {
    "subtitleLanguages": [],
    "cookieFileName": null,
    "rawArgs": [],
    "rawFormatCode": null
  }
}
```

响应应包括：

- 解析后的元数据
- 是否播放列表
- 格式候选摘要
- 重复检测结果

### 13.4 任务相关

- `POST /api/tasks`
- `GET /api/tasks`
- `GET /api/tasks/:taskId`
- `POST /api/tasks/:taskId/stop`
- `POST /api/tasks/:taskId/resume`
- `POST /api/tasks/:taskId/restart`
- `DELETE /api/tasks/:taskId`

`POST /api/tasks` 支持两类输入：

- 单视频任务
- 已解析播放列表的父任务与其子任务集合

### 13.5 下载库相关

- `GET /api/library/tree`
- `POST /api/library/refresh`
- `POST /api/library/folders`
- `POST /api/library/move`
- `DELETE /api/library/items`
- `POST /api/library/open-file`
- `POST /api/library/open-folder`

### 13.6 健康检查与诊断

- `GET /api/app/status`

建议返回：

- Bridge 版本
- 工具发现结果
- 当前下载根目录
- 当前活动任务数

---

## 14. SSE 事件模型

### 14.1 连接

- `GET /api/events`

浏览器启动后建立 1 条 EventSource 连接。

### 14.2 事件类型

V1 固定以下事件类型：

- `app.ready`
- `task.updated`
- `task.log`
- `task.removed`
- `playlist.updated`
- `library.changed`
- `settings.changed`
- `tool.status`

### 14.3 事件载荷示例

`task.updated`:

```json
{
  "taskId": "task_01H...",
  "state": "downloading",
  "progress": {
    "percent": 42.3,
    "speedBytesPerSec": 2310000,
    "etaSeconds": 82
  },
  "updatedAt": "2026-03-29T10:10:00Z"
}
```

`task.log`:

```json
{
  "taskId": "task_01H...",
  "attemptId": "attempt_02",
  "level": "info",
  "line": "[download] 42.3% of 348.12MiB at 2.30MiB/s ETA 01:22",
  "timestamp": "2026-03-29T10:10:00Z"
}
```

`library.changed`:

```json
{
  "reason": "task_completed",
  "paths": ["Video/Example.mp4"],
  "timestamp": "2026-03-29T10:10:00Z"
}
```

### 14.4 推送原则

- 任务进度变化时发送 `task.updated`
- 新日志行到达时发送 `task.log`
- 下载完成、删除、移动、创建文件夹时发送 `library.changed`
- 设置变更后发送 `settings.changed`

SSE 事件只做增量通知，前端仍可通过 HTTP 重新拉取完整状态。

---

## 15. 下载库操作设计

### 15.1 创建文件夹

流程：

1. 校验目标父目录在下载根目录内
2. 校验文件夹名称合法
3. 校验目标路径不存在
4. 创建目录
5. 更新索引
6. 广播 `library.changed`

### 15.2 移动

流程：

1. 校验源路径与目标路径都在下载根目录内
2. 校验目标不在源的子目录中
3. 校验目标同名冲突不存在
4. 校验文件未被活动任务占用
5. 执行移动
6. 更新索引
7. 修正相关任务输出路径
8. 广播 `library.changed`

### 15.3 删除

流程：

1. 校验路径位于下载根目录内
2. 校验活动任务占用情况
3. 删除文件或目录
4. 更新索引
5. 若关联任务已完成，则同步更新该任务输出文件清单
6. 广播 `library.changed`

---

## 16. sidecar 工具发现策略

### 16.1 查找顺序

工具查找顺序固定为：

1. 设置中的显式覆盖路径
2. Bridge 可执行文件同目录

V1 默认不回退到系统 PATH。

### 16.2 启动期诊断

应用启动时应记录：

- `yt-dlp` 是否可用
- `ffmpeg` 是否可用
- `ffprobe` 是否可用
- 工具版本字符串

若 `yt-dlp` 不可用，Bridge 不应允许开始解析或下载。

若 `ffmpeg` 或 `ffprobe` 不可用，Bridge 应允许启动，但需要在涉及后处理能力时给出明确错误。

---

## 17. 安全实现要点

### 17.1 路径安全

所有文件系统操作前都必须：

1. 拼出候选绝对路径
2. 做规范化或等价校验
3. 验证仍在下载根目录内

### 17.2 Cookie 文件

V1 建议将导入的 Cookie 文件复制到用户数据目录下的受控位置，再供任务引用。

原因：

- 避免任务直接依赖用户原始路径
- 降低路径失效问题
- 避免日志中暴露任意本地路径

### 17.3 日志脱敏

日志输出应避免直接暴露：

- 完整 Cookie 路径
- 潜在认证头
- 原始命令行中的敏感参数值

---

## 18. 最小实现切片

建议按以下顺序落地：

### M1

- Bridge 启动
- 内嵌单页 HTML 提供
- 设置读写
- 工具发现
- `/api/app/status`

### M2

- `resolve`
- 单视频入队
- 调度器
- `downloading -> paused -> resume -> restart -> completed`

### M3

- 任务日志
- SSE 事件
- 启动恢复
- 下载库索引与树查询

### M4

- 播放列表父子任务
- 创建文件夹
- 根目录内移动
- 删除与打开文件/目录

### M5

- 字幕
- Cookie 导入
- 原始参数覆盖
- 工具缺失场景的用户提示完善

---

## 19. 建议的下一步输出

在本文档基础上，下一步建议直接产出：

1. API DTO 明细文档
2. JSON 文件样例集
3. Rust 项目骨架
4. 内嵌 HTML 初版静态页面
