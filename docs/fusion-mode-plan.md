# scrcpy 4.0 融合模式调研与 Kulua 接入方案

> 目标仓库：`E:\code\sync-workspace`（即 `/mnt/e/code/sync-workspace`）
> 发布目录：`dist/sync-workspace`、`dist/kulua`
> 状态：✅ 已实施（2026-08-16），实施顺序见 §7，代码与文档已同步更新
>
> ⚠️ **方案变更（2026-08-16 同日）**：按用户要求，融合模式客户端**完全自研**，
> 不再使用官方 `scrcpy.exe`。§5/§6.2 的"复用官方 scrcpy.exe"决策已废弃，替换为：
> 新增 `fusion-viewer` crate（winit 窗口 + FFmpeg 软解 + 自研控制协议注入），
> daemon 的 `FusionManager` 拉起 `fusion-viewer.exe`（参数：serial/package/label/jar）。
> 服务器侧不变（`new_display` 虚拟显示器 + scid 隔离）；`build.ps1` 打包
> `fusion-viewer.exe` + FFmpeg DLL（avcodec-62/avutil-60/swresample-6），不再打包 scrcpy.exe。
>
> ⚠️ **方案变更（2026-08-16 晚）**：按用户要求，server 也**完全自研**（`kulua-server.jar`），
> 不再依赖官方 `scrcpy-server.jar`。§2/§5 的"内置官方 4.0 jar"决策已废弃，替换为：
> - 单 server 进程管理**多个虚拟显示器**：每个客户端 video 连接携带创建参数
>   （WxH/DPI），server 回 displayId + codecId 后推流（连接驱动，无 CREATE_DISPLAY 消息）
> - 每个连接首字节为类型握手：0x01=control / 0x02=audio / 0x03=video
> - 控制消息 touch/scroll/start_app/resize 带 displayId u32be 前缀（多显示器扩展）
> - 剪贴板：server 轮询系统剪贴板变化并推送（0x00 + len + UTF-8），control 连接
>   每连接独立线程（daemon session 与多个 viewer 可同时连接同一 server）
> - `list_apps=true` 一次性模式输出格式与官方一致，apps.rs 解析器复用
> - daemon session 部署 `kulua-server.jar`（参数仅 `scid=<hex>`），viewer 用
>   `--connect <port>` 连接 session 的 forward 端口，不再自部署 server
>
> ⚠️ **变更（2026-08-16）**：虚拟显示器分辨率跟随窗口物理尺寸（每秒轮询窗口尺寸
> 发 RESIZE_DISPLAY，替代不稳定的 Resized 事件），并新增 `--scale <系数>` 参数
> （默认 1.0，范围 0.1~4.0）：视频分辨率 = 窗口物理尺寸 × 系数，
> 系数 <1 降低编码分辨率换流畅度/带宽，>1 超采样更清晰。

---

## 1. 背景

希望让 Kulua 在已建立的设备会话中，选择手机上的应用，并在电脑上以 **scrcpy 4.0 融合模式** 打开独立窗口：

- 虚拟显示器（`--new-display`）
- 弹性显示器（`--flex-display` / `-x`）
- 像原生 PC 窗口一样缩放、控制、独立关闭

**核心约束（不可违背）**：

- 手机上绝不安装 APK，全部通过 `adb push` + `app_process` 实现
- 依赖官方 scrcpy-server（本项目已内置 4.0 jar）
- 保持现有剪贴板 / 通知 / 音频 session 不受影响

---

## 2. scrcpy 4.0 融合模式调研结论

已核对本地 `scrcpy v4.0` 源码（`/mnt/e/code/ref/scrcpy`，tag `v4.0`）与
`scrcpy-win64-v4.0` 官方包；本项目 `scrcpy-server` 的 SHA-256 与官方包完全一致。

### 2.1 关键能力

| 能力 | 参数 | 说明 |
|---|---|---|
| 新建虚拟显示器 | `--new-display[=WxH/DPI]` | 不镜像实体屏，创建独立 VirtualDisplay；Android 10+ |
| 弹性显示器 | `-x` / `--flex-display` | 窗口缩放时向 server 发 `TYPE_RESIZE_DISPLAY=21` 动态 resize |
| 启动应用 | `--start-app=<pkg>` | 连接后经控制通道发 `TYPE_START_APP=16` 到虚拟显示器 |
| 应用列表 | `--list-apps` | server 以 `list_apps=true` 一次性模式打印可启动应用后退出 |
| 独立窗口 | `--window-title=...` | 每个 scrcpy 进程一个原生窗口 |
| 外观/行为 | `--no-vd-system-decorations`、`--no-vd-destroy-content`、`--keep-active` | 控制虚拟显示器装饰、关窗行为、保持活跃 |

### 2.2 官方推荐示例

```bash
# 启动 Settings 的弹性显示器
scrcpy --new-display=1024x768 --start-app=com.android.settings --flex-display

# 完整推荐：H265、16M 码率、保持活跃
scrcpy --new-display -x --keep-active --start-app=org.videolan.vlc \
  --video-codec=h265 -b16M
```

### 2.3 关键内部机制

- 每个 scrcpy 客户端生成随机 `scid`，server socket 为 `localabstract:scrcpy_<scid>`
- 因此同一设备可并行运行多个融合窗口
- `-x` 模式下**不允许** `--window-width` / `--window-height` 和 `--crop`
- `-x` 模式默认初始虚拟显示器 `1280x960/160`
- `list_apps` 输出格式：
  - `*` 开头 = 系统应用
  - `-` 开头 = 普通应用
  - 每行 = 应用名（补空格到第 30 列）+ 空格 + 包名

### 2.4 协议常量（源码核实）

| 消息 | 方向 | type |
|---|---|---|
| `START_APP` | client → server | 16 |
| `RESIZE_DISPLAY` | client → server | 21 |

---

## 3. 现状差距

1. **Kulua 只跑 server，不跑桌面端**
   - 现有 session 使用 `video=false`，只做剪贴板 / 通知 / 音频
   - 没有视频解码 / 渲染 / 输入窗口

2. **缺少 scrcpy.exe 运行时**
   - `dist/sync-workspace` / `dist/kulua` 缺少：
     - `scrcpy.exe`
     - `SDL3.dll`
     - `avcodec-62.dll` / `avformat-62.dll` / `avutil-60.dll` / `swresample-6.dll`
   - 这些文件可从官方 `scrcpy-win64-v4.0` 打包引入

3. **现有 kill 逻辑会误杀融合窗口（最关键）**
   - `sync-core/src/scrcpy.rs`
   - `sync-core/src/session/handle.rs`
   - 目前使用 `kill -9 $(ps | grep com.genymobile.scrcpy ...)`
   - 会杀死设备上**所有** scrcpy 进程，包括以后打开的融合窗口

4. **没有应用列表 IPC 和 UI 入口**

---

## 4. 目标交互

- “在线设备”卡片：当 session 为 `running` 时显示“应用”按钮
- 点击后弹出应用选择框：
  - 搜索（应用名 / 包名）
  - 区分系统 / 普通应用
  - 支持手动刷新
- 选择应用 → daemon 启动一个 scrcpy 融合窗口
  - `--new-display -x --start-app=<pkg>`
- 可同时开多个应用窗口
- 不影响现有剪贴板 / 通知 / 音频 session

---

## 5. 总体方案

**复用官方 `scrcpy.exe` 作为融合窗口进程，不在 Rust 中自己实现视频解码 / 渲染 / 输入注入。**

理由：

- 工程量最小
- 保持“无 APK、只用 adb + scrcpy-server”约束
- 原生窗口体验最好
- 可复用官方 scrcpy 的缩放、输入、编解码、窗口管理能力

---

## 6. 详细设计

### 6.1 应用列表：server 一次性模式

新增 `sync-core/src/apps.rs`：

1. 复用已有 jar 部署逻辑，确保 `/data/local/tmp/scrcpy-server.jar` 存在
2. 执行一次性命令：

   ```bash
   adb -s <serial> shell CLASSPATH=/data/local/tmp/scrcpy-server.jar \
     app_process / com.genymobile.scrcpy.Server 4.0 \
     log_level=info list_apps=true
   ```

3. 设置超时（建议 20s），合并 stdout/stderr
4. 解析 `List of apps:` 之后的输出
5. 结果缓存（按设备 UUID，TTL 60s，断连/强制刷新失效）

不采用 `scrcpy.exe --list-apps` 的原因：

- 每次都要初始化 SDL + push jar，更重
- 我们已有 adb 与 jar 路径，server 一次性模式更可控

### 6.2 启动融合窗口

新增 `sync-core/src/fusion.rs`，每个应用启动一个 scrcpy 进程，推荐参数：

```bash
scrcpy.exe
  -s <serial>
  --new-display=1280x960/160
  -x
  --start-app=<package>
  --window-title=<应用名> — <设备名>
  --no-audio
  --no-clipboard-autosync
  --no-vd-system-decorations
  --keep-active
  --video-codec=h264
  --video-bit-rate=8M
  --port=27200:27299
```

设计决策：

| 参数 | 原因 |
|---|---|
| `--no-audio` | 音频继续由现有 Kulua session 统一转发，避免双采集冲突 |
| `--no-clipboard-autosync` | 剪贴板继续由 Kulua session 负责，避免多个 server 回环同步 |
| `--port=27200:27299` | 现有 session 端口从 27183 递增，融合窗口避开默认端口段 |
| 默认 H264 + 8M | 比 H265 兼容性更稳，后续可做设置项 |

`FusionManager` 职责：

- 查找 `scrcpy.exe`：daemon 同目录 → `SCRCPY_EXE` 环境变量 → PATH
- 生成窗口 id、保存 `Child`、构建参数（纯函数便于单测）
- 每 tick `try_wait` 回收已退出窗口，推 IPC 事件
- daemon 退出时优雅关闭窗口（Windows 优先 `taskkill /PID` 不带 `/F`，超时再强杀）

### 6.3 关键改造：scrcpy 进程隔离

现有 broad kill 必须替换，否则会杀死融合窗口。

- 给 Kulua 每个 session 生成确定性 `scid`：
  - 例如 `0x4B4C0000 | (port & 0xFFFF)`（`KL` 标记前缀）
- server 启动参数加 `scid=<hex>`
- ADB forward 目标从 `scrcpy` 改为 `scrcpy_<hex>`
- 删除三处 broad kill：
  - `scrcpy.rs::deploy_scrcpy` 启动前全量 kill
  - `scrcpy.rs::ScrcpyServer::stop`
  - `session/handle.rs::Handle::stop` 超时强杀
- 替换为按 scid 精确杀：
  - 遍历设备 `/proc/<pid>/cmdline`
  - 只 kill 参数含 `scid=<本会话 hex>` 的进程
  - 脚本中跳过 `$$`，防止 shell 自杀
- 部署前仅清理“同 scid 的陈旧进程”

效果：

- Kulua session 重启不会碰官方 scrcpy 融合窗口
- daemon 崩溃残留不会占用新 socket
- 多融合窗口互不干扰（官方各自随机 scid）

### 6.4 IPC 协议扩展

在 `sync-core/src/ipc/proto.rs` 扩展：

```proto
AppInfo { string package_name=1; string label=2; bool system=3; }

request.AppListParams { string uuid=1; bool force=2; }          // tag 17
request.AppOpenParams  { string uuid=1; string package_name=2; } // tag 18

response.AppList { repeated AppInfo apps=1; bool fusion_supported=2; } // tag 15
response.AppOpen { uint64 window_id=1; }                          // tag 16

event.AppWindowsUpdated { repeated AppWindowInfo windows=1; }     // tag 15
AppWindowInfo { uint64 window_id=1; string serial=2;
                string package_name=3; string label=4; string state=5; }
```

新增方法：

| Method | 行为 |
|---|---|
| `app.list` | 带 UUID；先查缓存，miss 时在 Core 中 spawn blocking 任务枚举，20s 超时 |
| `app.open` | 校验设备在线、包名合法、`ro.build.version.sdk >= 29`，spawn 后返回 `window_id` |
| `app.windows-updated` | 窗口启动 / 退出 / 失败时推送给 UI |

实现上给 `Command` 增加带 `oneshot::Sender` 的变体（`ListApps` / `OpenApp`），
让 IPC handler 能拿到同步结果，同时不阻塞 Core 主循环。

### 6.5 UI 改动

Tauri 新增：

- `get_apps(uuid, force)` → `app.list`
- `open_app(uuid, package_name)` → `app.open`
- 监听 `app-windows-updated` 事件

`ui/src/App.vue`：

- 设备卡片在 `sessionStates[uuid] === 'running'` 时显示“应用”按钮
- 新增应用选择弹层：
  - 加载态 / 错误态 / 空态
  - 搜索
  - “隐藏系统应用”开关
  - 手动刷新
  - 点击应用后 loading，成功提示“正在打开 xxx”，失败显示 daemon 错误

### 6.6 打包与发布

- 自研 viewer 运行时（无官方 scrcpy.exe）：
  - `fusion-viewer.exe`（winit 窗口 + FFmpeg 解码 + 自研控制协议）
  - FFmpeg DLL：`avcodec-62.dll` / `avutil-60.dll` / `swresample-6.dll`（vendor/scrcpy-win64）
- 自研 server jar：`kulua-server.jar`（`kulua-server/build.ps1` 产出，替代官方 scrcpy-server）
- `build.ps1` 自动打包 `fusion-viewer.exe` + FFmpeg DLL + `kulua-server.jar`
- `daemon/src/main.rs` 的 `find_jar()` 查找 `kulua-server.jar`（项目根/上级/daemon 同级）
- 更新 `README.md`、`IPC-DESIGN.md`、`AGENTS.md`

---

## 7. 实施顺序

| 阶段 | 内容 | 产出 |
|---|---|---|
| 0 | 合并 scrcpy v4.0 运行时到 dist、修 build 脚本 | 双击即可开融合窗口 |
| 1 | scid 会话隔离 + 精准 kill | 现有功能回归通过，不再误杀 scrcpy |
| 2 | `apps.rs` 应用枚举 + 解析单测 + 缓存 | daemon 可返回应用列表 |
| 3 | `fusion.rs` 启动 / 回收窗口 | daemon 可拉起 scrcpy.exe |
| 4 | IPC proto / server 路由 / Core 命令 | `app.list`、`app.open` 可用 |
| 5 | Tauri 命令 + Vue 应用选择器 | 从 session 一键打开应用 |
| 6 | 文档、边界处理、手工联调 | 完整功能 |

---

## 8. 测试计划

### 纯函数单测

- 应用列表解析（中文名、带空格名称、系统/普通应用、CRLF）
- scrcpy 参数构建
- 包名校验
- scid kill 脚本生成

### IPC 单测

- 新 proto 消息 roundtrip
- 缺参 / 非法 UUID / 未知包名错误

### MockAdb 测试

- 列表超时
- 设备离线
- jar 缺失时自动 push

### 手工联调

- Android 10 / 11 / 12+ 各一台
- 单设备多窗口、拖拽缩放、关闭窗口后 app 行为
- 融合窗口打开期间重启 Kulua session，确认窗口不消失
- 关闭 daemon 后窗口清理
- Android < 10 返回明确“不支持融合模式”

---

## 9. 默认决策

1. 融合窗口由 daemon 管理，GUI 关闭后窗口仍存在；daemon 退出时统一关闭
2. 第一版融合窗口静音，音频继续走现有 session
3. 默认 H264 + 8M，之后可在设置页扩展 H265 / 码率 / 系统装饰
4. 不做视频内嵌到 Tauri，使用 scrcpy 原生窗口
5. 保持“手机不装任何 APK”的硬约束不变

---

## 10. 参考资料

- scrcpy v4.0 源码：`/mnt/e/code/ref/scrcpy`（tag `v4.0`）
- 官方文档：`doc/virtual-display.md`、`doc/device.md`
- 当前项目文档：`README.md`、`IPC-DESIGN.md`、`AGENTS.md`、`scrcpy-protocol.md`
