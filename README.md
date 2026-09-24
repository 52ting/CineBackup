# CineBackup · 影视素材 / DCP 跨平台备份工具

Tauri 2 + Rust 实现的个人用备份工具。面向影视素材、DCP 包这类**大文件、跨文件系统**的场景：
源盘和目标盘可以是 APFS / NTFS / exFAT / FAT32 的任意组合，全程分块流式 IO，几十 GB 的
ProRes / MXF 也不会爆内存。

**下载**：[最新版本](https://github.com/52ting/CineBackup/releases/latest)
—— Windows 安装包 / macOS 通用 dmg，**无需登录**直接下。
**许可证**：[MIT](LICENSE) —— 可自由使用、修改、再分发（详见第十四节）。

---

## 一、目录结构与界面

```
cinebackup/
├── package.json                 # 前端依赖与脚本
├── vite.config.js               # Vite 配置
├── INSTALL-macOS.md             # ★ 面向非开发者的「装到 Mac」逐步操作指南（含报错对照表）
├── LICENSE                      # MIT 许可证（见第十四节）
├── index.html                   # 模块 1：Tauri 前端 UI（界面结构）
├── .github/workflows/
│   └── build-macos.yml          # ★ 没有 Mac 时用云端 macOS runner 出 dmg（见 5.3）
├── src/
│   ├── styles.css               # 界面样式（双主题：跟随系统 浅色/深色）
│   ├── backend.js               # 前端 ↔ Rust 的唯一桥梁（事件名/命令名集中处）
│   ├── ui.js                    # 纯渲染层（表格 / 进度 / 日志 / 模态框）
│   ├── dnd.js                   # ★ 拖放投放（拖入文件/文件夹，判定投放意图）
│   └── main.js                  # 交互装配层（按钮、对话框、事件订阅）
├── tools/
│   ├── build_windows.bat        # ★ Windows 一键打包（自动注入 MSVC 环境 + 预置打包工具）
│   ├── build_macos.sh           # ★ macOS 一键打包（出 .app + .dmg，支持通用二进制，见第五节）
│   ├── gh_release.py            # ★ 发布安装包到 GitHub Release（见第十三节）
│   ├── watch_ci.py              # ★ 盯云端构建 / 取产物（不用开浏览器、不用建 PAT，见 5.3）
│   ├── make_src_zip.py          # ★ 打包源码 zip（排除构建产物 / 日志），用于搬到 Mac 上编译
│   ├── verify_dmg.py            # 在 Windows 上校验 dmg 完整性（dmg 魔数 / FAT 头 / Info.plist）
│   ├── setup_bundler_tools.py   # ★ 镜像加速预置 WiX / NSIS（解决打包下载超时，见 6.4）
│   ├── preview_ui.py            # ★ 改完界面秒级截图核对（注入假后端，不用编译 Rust）
│   ├── debug_dom.py             # 把预览页 DOM dump 出来 + 抓控制台（排查前端事件用）
│   ├── clean_dist.py            # 清 dist（vite 设了 emptyOutDir:false，asset 会累积，打包前清）
│   ├── probe_mirrors.py         # 实测各 GitHub 镜像吞吐，挑最快的
│   ├── gen_icons.py             # 零依赖生成占位图标（PNG/ICO/ICNS）
│   └── env_check.py             # 环境自检（工具链 / 磁盘空间 / 安装位置）
└── src-tauri/
    ├── Cargo.toml
    ├── build.rs
    ├── tauri.conf.json          # 窗口 / 打包配置
    ├── capabilities/default.json# 权限声明（核心 + 文件对话框）
    ├── tests/
    │   ├── resume_rule.rs       # ★ 断点续传规则的端到端测试（真实文件落盘）
    │   └── verify_progress.rs   # ★ 校验阶段的进度上报频率（边读边发 / 边界不丢）
    └── src/
        ├── main.rs              # 二进制入口
        ├── lib.rs               # 模块声明 + Tauri Builder
        ├── types.rs             # 前后端共享数据结构
        ├── util.rs              # 分块常量、路径工具、速率/ETA 估算
        ├── state.rs             # 全局状态（单任务互斥 / 取消 / 弹窗回信）
        ├── events.rs            # 事件契约（载荷结构 + 阻塞式 ask_user）
        ├── fsinfo.rs            # 模块 2：磁盘文件系统类型探测
        ├── disks.rs             # ★ 磁盘/卷枚举（自动拉取本机硬盘，Windows 盘符 / macOS 卷）
        ├── walk.rs              # 模块 6：文件/目录判断 + 目录遍历
        ├── hash.rs              # 模块 5：分块流式内容哈希（默认 SHA-256，可切 xxHash64）
        ├── copy.rs              # 模块 4：原生分块拷贝 + 断点续传 + Dry Run
        ├── scan.rs              # 模块 3：冲突预扫描（带扫描进度）
        ├── verify.rs            # 校验阶段（SHA-256 逐文件比对，值可对外核对）
        ├── engine.rs            # 任务调度引擎（后台线程主流程）
        ├── task.rs              # 模块 7：JSON 任务保存 / 加载
        └── commands.rs          # Tauri 命令入口
```

### 界面布局（三栏：备份源 / 本机磁盘·进度 / 目标文件夹）

参考 Carbon Copy Cloner 的选盘交互重做：**磁盘只在中间拉一份**，往左投放=加源、往右投放=设目标。

```
┌───────────────────────────────────────────────────────────────┐
│ CineBackup   [容量胶囊：源 12.4 GB ▸ 需写入 8.1 GB]   [状态]   │
├──────────────┬───────────────────────────────┬────────────────┤
│ 备份源 [n]   │ 磁盘 / 传输    ↻ 刷新  ⌄ 切换  │ 目标文件夹     │
│ ┄┄虚线投放┄┄  │  ┌─────────┐  ┌─────────┐     │ ┄┄虚线投放┄┄   │
│ ▸ 源卡片     │  │ ▤ C: 系统│  │ ▤ D: 素材│ ... │ ▸ 目标卡片     │
│ ▸ 源卡片     │  └─────────┘  └─────────┘     │  路径 / 文件系统│
│              │  （任务开始后本区切换成 ↓）    │  剩余空间 / 只读│
│              │  A001  ████████░░ 72%  1.2 GB/s│                │
│              │  A002  ██░░░░░░░░ 18%  等待中   │                │
│              ├───────────────────────────────┤                │
│              │ 总进度 ██████░░░░  开始 / Dry  │                │
├──────────────┴───────────────────────────────┴────────────────┤
│ 运行日志                       │ 校验结果（✅/❌/⏭ + SHA-256） │
└───────────────────────────────────────────────────────────────┘
```

| 栏位 | 内容 | 交互 |
|---|---|---|
| 左 · 备份源 | 已加入的源卡片（路径 + 文件系统 + 大小） | 虚线区可拖入；卡片上可移除 |
| 中 · 本机磁盘 | **单份**磁盘网格（不再左右各拉一遍） | 点磁盘弹菜单：**整盘加入 / 挑选文件夹 / 设为目标** |
| 中 · 传输列表 | 任务开始后自动切换为「一源一行」的进度列表 | 每行独立进度条 + 当前文件名 + 速度 / 剩余 |
| 右 · 目标文件夹 | 目标路径、文件系统、剩余空间（只读盘给警示） | 虚线区可拖入；点磁盘卡换目标 |
| 底部 · 校验结果 | 状态 / 文件 / 大小 / **校验值（SHA-256）** / 说明 | 校验值那格**单击即全选**，可 Ctrl+C 取走完整 64 位哈希 |

- **中栏是双态的**：空闲显示磁盘网格，开始备份即切到传输列表；点中栏右上角的 **⌄**
  可以随时在两者之间来回切（**任务运行中也能切回磁盘看剩余空间**，总进度条会一直留在下面）。
  箭头当前处于「可切换」状态时会高亮；空闲且没有传输行时，它退化成「收起 / 展开磁盘网格」。
- **每源一行进度来自后端**：扫描阶段给每个 `PlanItem` 打上 `src_idx`（它属于第几个源），
  拷贝阶段维护一份 `SourceProgress` 累加器，随 `ProgressReport.sources` 一起下发。
  `state = waiting / active / done / failed` 都是真实状态，不是前端猜的。
  拷贝是串行的，所以同一时刻只有一行 `active`，它前面的行都是 `done`。
- 校验阶段 `sources` 是空数组，前端**保留拷贝阶段的行**、只换相位文案，不会闪回空列表。
- **运行中还能继续加源 → 自动追加一轮**：任务跑着的时候把新素材拖进左栏（或用左栏的「＋」），
  源列表立刻更新，并自动排一轮追加备份 —— 本轮 `JOB_END` 之后自动再跑一次
  （「开始备份」按钮此时显示为琥珀色 **取消排队 ⌛**，点一下即可撤销）。
  之所以排队而不是并行：后端 `AppState.busy` 同时只允许一个任务；而备份本身是幂等的
  （已备份的文件按断点续传规则跳过），重跑一轮代价很小。用户主动「取消任务」的那一轮不追加。
- **双主题**：`:root` 为浅色变量，`@media (prefers-color-scheme: dark)` 与
  `:root[data-theme="dark"]` 覆盖为深色。磁盘图标是纯 CSS 画的扁平轮廓
  （`--ico-face` / `--ico-base`），深浅两套分别校准过对比度，没有用滤镜。

### 磁盘自动拉取（`disks.rs` + 中栏磁盘网格）

中栏顶部**只有一条「本机磁盘」**，启动时自动拉取，不需要手动去文件系统里翻。

| 平台 | 实现 | 拿到的信息 |
|---|---|---|
| Windows | `GetLogicalDriveStringsW` → `GetDriveTypeW` → `GetVolumeInformationW` → `GetDiskFreeSpaceExW` | 盘符、卷标、NTFS/exFAT/FAT32、总容量、剩余、介质类型、只读标志 |
| macOS | 根卷 + `/Volumes/*`，逐个 `statfs(2)` | 挂载点、卷名、APFS/HFS+/exFAT、容量、`MNT_RDONLY` 只读标志 |

交互：

- **点任意磁盘 → 弹出小菜单**：`整盘加入为源` / `挑选文件夹` / `设为目标`
  （磁盘面板本身不再分左右，靠菜单决定这次是加源还是设目标）
- **「整盘加入」** → 把盘根目录整个作为一个源；**「设为目标」** → 在该盘内选目标文件夹
- 只读卷（macOS 原生 NTFS）设为目标时会拒绝并给出提示
- 已在用的盘会高亮；容量条 ≥78% 转橙、≥90% 转红；掉线盘（剩余 0）标记为不可用
- 刷新时机：启动、点「↻ 刷新」、窗口重新获得焦点、每 6 秒轻量轮询（仅窗口可见且无任务在跑）
- 轮询时若磁盘集合没变，**只更新容量文本不重建 DOM**，避免闪烁和打断鼠标悬停

> 未插入介质的光驱 / 读卡器会被自动跳过（`GetVolumeInformationW` 直接失败）。

### 拖入文件 / 文件夹（`dnd.js`，拖放投放）

直接从资源管理器 / Finder 把文件或文件夹**拖进窗口**即可，不用一层层点目录。
Tauri 会接管 webview 的原生拖放，通过 `tauri://drag-*` 事件把**绝对路径**交给前端，
所以拖进来的就是真实磁盘路径，能直接喂给后端的 `probe_path` / `add_source`。

```
   ┌───────────────┬───────────────────────────┬───────────────┐
   │ 备份源        │  本机磁盘 / 任务进度       │ 目标文件夹     │
   │ ← 高亮蓝框     │  （默认算加源，左栏点亮）  │ ← 高亮绿框     │
   └───────────────┴───────────────────────────┴───────────────┘
     拖到这里加源        其它区域默认加源          拖到这里设目标
```

> 判定只看「是否落在右栏 `.area-dst` 内」：是→设目标，否则一律按加源处理。

| 投放位置 | 行为 |
|---|---|
| 左侧「备份源」栏 | 拖入的每一项都加入源列表（文件 / 文件夹混着拖也行，自动去重） |
| 右侧「目标文件夹」栏 | 第一个文件夹直接设为目标；只拖了文件则取其所在文件夹设为目标 |
| 其它区域 | 默认按「加入备份源」处理 |
| 任务运行中 · 左栏 / 其它区域 | **照常加源**，并自动排一轮追加备份（见上） |
| 任务运行中 · 右栏 | 拒绝（遮罩转橙色警示），避免把正在写入的位置换掉 |

实现要点：

- 投放意图由**鼠标位置**判定，用 `elementFromPoint` 找所在卡片
  （遮罩层设了 `pointer-events: none`，不会挡住命中测试）。
- ⚠️ **坐标单位两个平台不一样**，这是踩过的坑：wry 在 macOS 用
  `NSPoint draggingLocation()`（AppKit **逻辑点**），在 Windows 用
  `ScreenToClient()`（客户区**物理像素**）。早先一律除以 `devicePixelRatio`，
  于是在 Retina Mac（dpr=2）上右栏 x≈950 被折半成 475、落进中栏，
  命中判定兜底返回 `source` —— **「拖到目标栏」被当成「加到源」**。
  现在按平台取尺度（macOS 用 1、其余除以 dpr），换算后明显出界时再换另一个尺度兜一次，
  最后还有一层「离哪一栏更近」的几何兜底。预览工具通过 `window.__CB_DND_SCALE__ = 1`
  固定尺度（它投递的本来就是 CSS 像素）。
- 拖拽期间底部弹出提示卡，显示本次要放什么（最多列 3 条路径 + 「另外 N 项」），
  并高亮命中的卡片、把另一侧压暗；运行中拖到左栏时提示卡会说明「跑完自动补跑新加的源」。
- `over` 事件不带 `paths`，所以用 `enter` 带着的路径缓存起来给提示卡用；
  最终落点以 `drop` 事件**自带**的坐标为准（`over` 可能稀疏 / 缺失）。
- 看门狗：超过 2.5 秒没有新事件就自动收起遮罩，兜住个别平台漏发 `leave` 的情况。
- 订阅失败（非 Tauri 环境）时静默降级成 `console.warn`，并在启动日志里提示「拖放功能未启用」。

> 想在不编译 Rust 的情况下核对拖拽界面：
> `python tools/preview_ui.py --shot --dnd --dnd-x 300 --dnd-y 300`（左栏）/
> `--dnd-x 1150 --dnd-y 300`（右栏）；加 `--drop` 会真的松手投递一次，用来验证落点判定。

### 已验证状态（Windows 10 22H2 / rustc 1.98.1）

| 项目 | 结果 |
|---|---|
| `cargo check --all-targets` | ✅ 0 error 0 warning |
| `cargo test` | ✅ 33 passed / 0 failed（16 单元 + 15 续传 + 2 校验进度） |
| `vite build` | ✅ 产出 `dist/` |
| `npm run tauri build` | ✅ 产出 MSI + NSIS 两个安装包（见 6.3） |
| 磁盘自动拉取 | ✅ 实机检测到 7 个卷（含 3 个映射网络盘），容量/只读标志正确 |
| 三栏界面 · 双主题 | ✅ 预览工具各截过 4 个状态（浅/深 × 空闲/运行中），见 3.1 |
| 工具链 | rustc/cargo 1.98.1 · VS 2022 BuildTools 17.14.41 · Windows SDK 10.0.26100 |

复跑测试：

```bash
cd src-tauri
cargo test --test resume_rule -- --nocapture     # 断点续传 15 条
cargo test --test verify_progress -- --nocapture # 校验阶段的进度上报 2 条
```

> 测试里的文件都是真实落盘的（在系统临时目录），不是 mock。
> 其中 `t2` 刻意把目标截在「非 4 MiB 整块边界」上，专门覆盖「上次拷到一半被强杀」的真实场景。

---

## 二、环境准备

### 通用

| 组件 | 版本 | 说明 |
|---|---|---|
| Rust | ≥ 1.77（stable） | `rustup` 安装 |
| Node.js | ≥ 18（推荐 20/22） | 前端构建 |

```bash
# 安装 Rust（若未安装）
# macOS / Linux
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Windows：到 https://rustup.rs 下载 rustup-init.exe 安装，选 MSVC 工具链
```

### macOS 额外依赖

```bash
xcode-select --install          # Xcode Command Line Tools
```

### Windows 额外依赖

- **Microsoft C++ Build Tools**（勾选「Desktop development with C++」）
- **WebView2 Runtime**：Win10 1803+ / Win11 一般已内置；
  安装包已配置 `downloadBootstrapper`，缺失时会自动引导安装。

---

## 三、开发运行

```bash
cd cinebackup
npm install
npm run tauri dev          # 启动开发模式（改前端热更新，改 Rust 自动重编译）
```

> 首次 `cargo build` 需要编译 Tauri 依赖，耗时约 3–10 分钟，属正常现象。

### 3.1 只改界面时：秒级预览，不编译 Rust

改样式 / 布局最快的方式是走预览工具——它把 `dist/` 复制一份、注入「假后端」
（假的磁盘列表、假的文件对话框），再用本机 Chrome 无头模式截图：

```bash
npm run build                              # 只构建前端，约 1 秒
python tools/preview_ui.py --shot          # 生成 tools/ui-preview-light.png（浅色 · 空闲）
python tools/preview_ui.py --serve         # 或者起 http://127.0.0.1:8765 自己开浏览器点
```

几个常用开关（可组合，输出文件名会自动带上后缀）：

| 开关 | 作用 |
|---|---|
| `--theme dark` / `--theme light` | 强行走 `?theme=` 对应的配色（默认跟随系统） |
| `--run` | 模拟点「开始备份」：发状态 / 计划 / 进度事件，中栏切成传输列表 |
| `--scan` | 模拟**预扫描进行中**：中栏当前那行显示流动滑块 + 正在扫描的文件名，其余「预扫描中…」 |
| `--verify` | 模拟**校验阶段进行中**：核对底部百分比精度与「当前文件」进度（数字取自真机截图：667 GB / 64 MB/s） |
| `--run --fold` | 接着再点一次中栏箭头，截「运行中切回磁盘视图」的样子 |
| `--dnd --dnd-x N --dnd-y N` | 模拟拖拽悬停，核对高亮框与命中判定（左栏 ≈300、右栏 ≈1150） |
| `--dnd --drop` | 悬停 0.9 秒后真的松手投递一次，用来验证「落点到底判给了哪一侧」 |
| `--probe` | 跑 `?queuetest=1` 回归场景（运行中加源 → 追加轮），用无头 Chrome dump DOM 后打印判定 JSON，**不截图** |
| `--queuetest` | 配合 `--shot`：把「追加轮进行中」的样子截下来（上一轮的行与校验结果都还在） |
| `--width` / `--height` | 窗口尺寸（默认 1280×880） |

```bash
python tools/preview_ui.py --shot --theme dark --run       # tools/ui-preview-run-dark.png
python tools/preview_ui.py --shot --theme light            # tools/ui-preview-light.png
python tools/preview_ui.py --shot --run --fold             # tools/ui-preview-run-fold-light.png
python tools/preview_ui.py --shot --scan --theme dark      # tools/ui-preview-scan-dark.png
python tools/preview_ui.py --shot --verify                 # tools/ui-preview-verify-light.png
python tools/preview_ui.py --shot --dnd --drop --dnd-x 1150  # 右栏松手 → 应设为目标
python tools/preview_ui.py --shot --run --dnd --drop --dnd-x 300  # 运行中拖入左栏 → 应自动排队
python tools/preview_ui.py --probe                         # 追加轮只补新源（打印 verdict 判定）
python tools/preview_ui.py --shot --queuetest              # tools/ui-preview-queuetest-light.png
```

`--probe` 是**回归探针**：脚本包一层 `window.__TAURI_INTERNALS__.invoke`，记下每次
`start_job` 实际发出去的源，最后打印一批判定。目前断言四件事：

| 断言 | 为什么钉住它 |
|---|---|
| `第二轮是否只含新源` | 带全源重跑会让预扫描把上一轮刚写完的内容再读两份（几小时空转），见第七节 |
| `中栏行数` = 7（3 老 + 4 新） | 追加轮整列替换会让上一轮的行消失 |
| `结果表是否保留上一轮的校验值` | 追加轮清空结果表 = 用户看到「先拷的那个文件没有校验」 |
| `第一轮是否被误标为追加` | 日志措辞串台会让人误以为第一轮也没全跑 |

0.4.3 那个「排队了却没拷新素材」的 bug 就是先被它定位到「前端没问题、第二轮确实发了」
（`startCallCount: 2`），再去后端找到真正的空转读取。

假后端会返回 5 块盘（含 1 块映射网络盘、1 块剩余为 0 的掉线盘），并自动演一遍
「加两个源 → 选一个目标」，所以截出来的是有内容的真实状态，不是空壳。

> 这套假后端只存在于 `.preview/`，**不会进产物**。
> 布局定稿后再 `npm run tauri build`，那一步才会把前端嵌进 exe。
>
> 预览脚本对 `.preview/` 用**增量同步**而不是 `rmtree`：本机宿主对批量删除有配额限制
> （单回合 50 个文件），整目录删会被拦掉；同理 `vite.config.js` 里设了 `emptyOutDir: false`，
> 构建前要清 `dist` 请手动跑 `npm run clean`。
>
> 副作用要知道：`emptyOutDir: false` 意味着 `dist/assets/` 会**同时留着新旧 hash 的文件**
> （比如同时存在 `index-Bbrp6bj3.js` 与上一版 `index-DUl211eY.js`），
> 而 `tauri build` 会把整个 `dist/` 都嵌进 exe —— 所以打进包的 exe 里能看到多个 asset 名字。
> `index.html` 引用的始终是最新那个，功能不受影响，只是白多几十 KB。想干净就先 `npm run clean`。

⚠️ 注意打包顺序：`tauri build` 会在开头跑一次 `vite build`。
如果打包启动**之后**才改前端，产出的安装包里还是旧界面——改完前端要重新打包一次。

> ⚠️ **别用「grep exe 找 asset hash」来判断前端新鲜度**（实测证伪）：Tauri 2 默认开启
> `compression`，内嵌的前端资源整体被 brotli 压缩，exe 里既搜不到 `index-XXXX.js`，
> 也搜不到 `optAlgo` 这类前端独有字符串（连 `index-` 前缀的命中数都是 0）。
> 能搜到的只有 Rust 侧的明文（比如 `SHA-256`、报错文案）。
> 可靠判据只有两个：① 打包前 `ls dist/assets` 只有当前这一对文件；② 重编一次看字节尺寸是否变化。

---

## 四、图标（打包前必做一次）

`tauri.conf.json` 里引用了 `src-tauri/icons/` 下的图标文件，仓库里**不含二进制图标**，
打包前用一张 1024×1024 的 PNG 生成全套：

```bash
cd cinebackup
npm run tauri icon ./your-logo-1024.png
```

该命令会自动生成 `32x32.png`、`128x128.png`、`128x128@2x.png`、`icon.icns`（macOS）、
`icon.ico`（Windows）等，路径与配置一致，无需手工调整。

---

## 五、macOS 编译与打包

> ⚠️ **必须在一台 Mac 上做**。Tauri 不支持交叉编译，Windows 上打不出 `.app` / `.dmg`，
> 反之亦然。没有 Mac 的话可以走 `.github/workflows/build-macos.yml`（见 5.3）。

### 5.1 一键脚本（推荐）

> 不想看技术细节的话，直接照 **`INSTALL-macOS.md`** 一步步做就行 ——
> 那份是给非开发者写的，含依赖安装、报错对照表、分发给别人的步骤。

源码拷到 Mac 后，先补齐三样依赖，再跑脚本：

```bash
# 1) Xcode Command Line Tools（提供 linker / SDK）
xcode-select --install

# 2) Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 3) Node.js ≥ 18
brew install node

# 4) 开打
cd cinebackup
bash tools/build_macos.sh                 # 本机架构，出 .app + .dmg
UNIVERSAL=1 bash tools/build_macos.sh     # 通用二进制（Intel + Apple Silicon）
BUNDLES=dmg  bash tools/build_macos.sh    # 只要 dmg
```

脚本会依次做：环境自检（缺什么直接告诉你装什么）→ 图标兜底 → `npm install` → `tauri build` → 列产物。

### 5.2 手动命令（等价）

```bash
cd cinebackup
npm install

npm run tauri build                                        # 本机架构
rustup target add x86_64-apple-darwin aarch64-apple-darwin # 通用二进制需先补 target
npm run tauri build -- --target universal-apple-darwin
npm run tauri build -- --bundles app                       # 只要 .app，不要 dmg
```

产物位置（Apple 架构后缀会是 `aarch64` / `universal`）：

```
src-tauri/target/release/bundle/
├── macos/CineBackup.app
└── dmg/CineBackup_0.4.1_x64.dmg

# 通用二进制会落在另一个 target 目录：
src-tauri/target/universal-apple-darwin/release/bundle/
```

> macOS 打包**不需要联网下载工具链**（用系统自带的 `hdiutil`），
> 所以第 6.4 节那个 Windows 上的 `timeout: global` 坑在这里不会出现。

### 5.3 没有 Mac：用 GitHub Actions 出包（推荐）

仓库：**https://github.com/52ting/CineBackup**（**公开**，源码与产物无需登录即可访问）。
`.github/workflows/build-macos.yml` 已配好三种触发方式：

| 触发 | 说明 |
|---|---|
| 推送到 `main` | 自动出**通用二进制**包（只改 `*.md` / `tools/**` 等不参与构建的文件的提交不触发，省得白等 9 分钟） |
| 推 `v*` tag（如 `v0.4.0`） | 自动出通用二进制包；tag 推送不受 paths 过滤影响，一定会跑 |
| Actions 页面 **Run workflow** | 手动触发，`universal` 勾掉则只打本机架构（省一半时间） |

产物在本 run 的 **Artifacts** 区下载：`CineBackup-macOS-universal`
→ 解开得到 `dmg/CineBackup_<版本>_universal.dmg` + `macos/CineBackup.app`。
实测首次（无缓存）通用构建约 **9 分钟**；`swatinem/rust-cache` 命中后会明显更快。

> ⚠️ **装的时候用 dmg，不要直接把 zip 里那个 `.app` 拖出来** —— GitHub 产物打包会丢掉
> 可执行权限位，拖出来的 App 可能起不来；dmg 内部的权限是完整的。
> ℹ️ 仓库已转为**公开**，公开仓库的标准 runner（**含 macOS**）用量**完全免费、不计费**，
> 之前「私有仓库 macOS 10 倍扣额度、约 22 次/月」的压力没有了。
> 不过 `paths-ignore` 仍然保留 —— 纯文档 / 工具脚本的改动不该白跑一次 9 分钟的构建。
> 要出 Windows 安装包仍走第 6 节的本机打包。

**在 Windows 上查构建状态 / 下载产物**（不必开浏览器，也不用建 PAT）：
本机 Git for Windows 的 Credential Manager 已存有推送时的授权，`tools/watch_ci.py`
用它取回令牌在内存里调 API：

```bash
python tools/watch_ci.py status         # 看最新一次运行的每一步状态与耗时
python tools/watch_ci.py watch          # 盯到结束，成功则自动下载并解出 .dmg
python tools/watch_ci.py logs <run_id>  # 构建失败时抓日志尾部
```

### 5.4 不装也能先验包（Windows 上就能做）

拿到 dmg 先别急着发出去，`verify_dmg.py` 靠**读字节**就能验三件事 —— 不用挂载、不用 macOS：

```bash
python tools/verify_dmg.py                       # 自动挑 cinebackup-builds/ 里最新的产物
python tools/verify_dmg.py path/to/xxx.dmg       # 也可以指定（会顺带解开同目录的产物 zip）
```

| 验什么 | 怎么验 |
|---|---|
| dmg 是不是合法磁盘映像 | 尾部 512 字节以 `koly` 开头（UDIF 资源尾巴的魔数） |
| 是不是**通用二进制** | `.app` 可执行文件头 4 字节 `0xCAFEBABE` = FAT_MAGIC，再逐条读架构表（`0x01000007`=x86_64 / `0x0100000C`=arm64） |
| Info.plist 关键字段 | 从 plist 里搜 `CFBundleIdentifier` / `CFBundleShortVersionString` / `LSMinimumSystemVersion` |

失败时**退出码为 1**，可以直接串进发布脚本。典型输出：

```
[dmg] CineBackup_0.4.4_universal.dmg
      尾部魔数  : b'koly'  ✓ 合法 UDIF 磁盘映像
[zip] CineBackup-macOS-universal.zip  条目 4
      可执行文件      ✓ FAT universal，含 2 个架构
        ├ x86_64 (Intel)         切片   5.63 MB（偏移 4096）
        ├ arm64 (Apple Silicon)  切片   5.21 MB（偏移 5914624）
[plist] zip 内 macos/CineBackup.app/Contents/Info.plist
      标识符     : com.ronnie.cinebackup
      版本      : 0.4.4
      最低系统    : 10.15
```

> ⚠️ 两点容易踩：① dmg 容器本身**读不到里面的 `.app`** —— 想验 FAT / Info.plist
> 必须有同目录的产物 zip（`watch_ci.py watch` 会一起拉下来），脚本会自动去找；
> ② 产物目录是**仓库同级**的 `cinebackup-builds/`（与 `watch_ci.py` 的落点一致），
> 不是工作区根目录下那个同名文件夹。

### 5.5 装到本机

```bash
cp -R "src-tauri/target/release/bundle/macos/CineBackup.app" /Applications/
xattr -dr com.apple.quarantine /Applications/CineBackup.app
open /Applications/CineBackup.app
```

**本工具仅个人自用，未做代码签名与公证**，所以首次打开一定报「已损坏」或「无法验证开发者」，
必须执行上面那条 `xattr`（或去「系统设置 → 隐私与安全性 → 仍要打开」点一下）。
把 dmg 里的图标拖进「应用程序」也一样，拖完照样要跑一次 `xattr`。

### 5.6 macOS 上的其他注意事项

| 事项 | 说明 |
|---|---|
| 首次访问受保护目录 | 拖入「桌面 / 文档 / 下载」里的文件时，系统会弹授权框，允许即可（macOS 的 TCC） |
| NTFS 目标盘 | 原生驱动**只读**，`disks.rs` 会把该卷标成 `writable=false`，界面上显示「只读」并拒绝设为目标；要写入需装 Paragon / Tuxera / Mounty |
| exFAT / FAT32 | 双平台可读写，跨平台素材盘首选（FAT32 单文件上限 4 GB） |
| 系统盘 `/` | macOS 11+ 的 `/` 是只读系统卷，会显示「只读」，属正常现象，别拿它当目标 |
| 元数据过滤 | 遍历时自动跳过 `.DS_Store` / `._*` / `.Spotlight-V100` / `.Trashes` / `.fseventsd` 等，Windows 上不做这个过滤 |

---

## 六、Windows 编译与打包

### 6.1 一次性环境准备

Windows 上编译 Tauri 需要三样东西，缺一不可：

| 组件 | 用途 | 安装方式 |
|---|---|---|
| Rust（`stable-x86_64-pc-windows-msvc`） | 编译后端 | `winget install Rustlang.Rustup` |
| **MSVC C++ 生成工具**（含 `link.exe`） | 链接阶段必需 | `winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"` |
| **Windows SDK** | 提供 `dbghelp.lib` 等系统库、`rc.exe` | 随上面的 VCTools 工作负载一并安装 |

> 不要用 `--includeRecommended` 之外的选项去精简；上面这条命令已验证可产出可用的 `cl.exe` / `link.exe` / SDK。
> 装完约 3–4 GB（BuildTools 目录）。

### 6.2 ⚠️ 本机特有坑：`LNK1181: 无法打开输入文件 dbghelp.lib`

**症状**：`cargo build` 时**所有** crate 的 build script 同时链接失败，报错都是
`LINK : fatal error LNK1181: 无法打开输入文件"dbghelp.lib"`。

**根因**：本机（Windows 10 22H2 / 19045）的 Windows SDK 只注册在 **32 位注册表视图**
（`HKLM\SOFTWARE\WOW6432Node\Microsoft\Microsoft SDKs\Windows\v10.0`），而
64 位的 `rustc` 读的是 **64 位视图**（`HKLM\SOFTWARE\Microsoft\Microsoft SDKs\Windows\v10.0`）——
那条键不存在，所以 rustc 定位不到 SDK 的 `Lib\...\um\x64`，也就传不给 `link.exe`。

验证方法：

```bash
# 32 位视图有内容
reg query "HKLM\SOFTWARE\WOW6432Node\Microsoft\Microsoft SDKs\Windows\v10.0"
# 64 位视图报「系统找不到指定的注册表项」
reg query "HKLM\SOFTWARE\Microsoft\Microsoft SDKs\Windows\v10.0"
```

**解决**：用微软官方的 `vcvars64.bat` 显式注入 `LIB` / `PATH`，绕开注册表探测。

**最省事的方式**：直接双击项目里的包装脚本，它会自动做完全部步骤：

```
tools\build_windows.bat
```

如果想在命令行里手动跑，等价写法是在 PowerShell / cmd 中：

```cmd
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
cd /d C:\path\to\cinebackup
npm run tauri build
```

在 Git Bash 里则用环境变量等效替代（`LIB` 为分号分隔的 Windows 路径）：

```bash
export LIB='C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207\lib\x64;C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\ucrt\x64;C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\um\x64'
export PATH="/c/Program Files (x86)/Windows Kits/10/bin/10.0.26100.0/x64:$PATH"
cargo build          # 或 cargo test / npm run tauri build
```

> `LIB` 与 `PATH` 里的版本号（`14.44.35207`、`10.0.26100.0`）随安装版本变化，
> 用前请核对 `ls "C:/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/VC/Tools/MSVC/"`。
> 这也是更推荐用 `build_windows.bat` 的原因——它调用 `vcvars64.bat` 自动探测，不用手写版本号。

### 6.3 打包

```powershell
# 默认同时产出 MSI + NSIS 安装包
npm run tauri build

# 只出某一种
npm run tauri build -- --bundles msi
npm run tauri build -- --bundles nsis
```

**最省事的方式**：双击 `tools\build_windows.bat`。它会依次完成
「注入 MSVC 环境 → 预置 WiX/NSIS → npm install → tauri build」全流程。

产物位置（**以下为实机打包验证过的真实结果**）：

```
src-tauri\target\release\
├── cinebackup.exe                              8.07 MB   免安装，可直接双击
└── bundle\
    ├── msi\CineBackup_0.4.1_x64_en-US.msi      2.8x MB   MSI 安装包
    └── nsis\CineBackup_0.4.1_x64-setup.exe     1.8x MB   NSIS 安装包
```

> 改动前端界面后**务必先把版本号 +1**（`package.json`、`src-tauri/Cargo.toml`、
> `src-tauri/tauri.conf.json` 三处一致），否则 MSI 同版本覆盖安装会出现「已安装」的迷惑提示。

### 6.4 ⚠️ 本机特有坑：`failed to bundle project: timeout: global`

**症状**：Rust 主程序已编译成功、打印了 `Built application at: ...\cinebackup.exe`，
但紧接着在打安装包这一步失败：

```
Info Verifying wix package
 Downloading https://github.com/wixtoolset/wix3/releases/download/wix3141rtm/wix314-binaries.zip
failed to bundle project: `timeout: global`
```

**根因**：打包 .msi / .exe 安装包需要外部工具链，tauri 会临时从 GitHub Release 下载：

| 工具 | 用途 | 下载地址 |
|---|---|---|
| WiX 3.14 | 生成 `.msi` | `wixtoolset/wix3` releases |
| NSIS 3.11 + 插件 dll | 生成 `setup.exe` | `tauri-apps/binary-releases`、`tauri-apps/nsis-tauri-utils` |

**本机直连 GitHub Release 资产的吞吐只有约 176 KB/s**（WiX 压缩包 39.4 MB，直连要 3 分多钟），
而 tauri 内置的 HTTP 客户端约 1 分钟就放弃 → `timeout: global`。

**注意**：这不代表网络不通。用单字节 `Range` 请求探测会误判成「很快」
（0.8 秒返回，但那只是响应头），必须实测**吞吐**才看得出被限速。

**解决**：用国内镜像预先把工具链放到 tauri 的缓存目录，之后 tauri 直接复用、不再联网下载：

```bash
# 自动完成下载 + SHA 校验 + 解压 + 部署
python tools\setup_bundler_tools.py          # WiX + NSIS 都装
python tools\setup_bundler_tools.py wix      # 只装 WiX（只打 msi 时够用）
python tools\setup_bundler_tools.py nsis     # 只装 NSIS
```

部署位置：

```
%LOCALAPPDATA%\tauri\
├── WixTools314\              # 需含 candle.exe / light.exe / wix.dll ...
└── NSIS\nsis-3.11\           # 需含 makensis.exe / Bin\ / Include\ / Stubs\ ...
```

实测镜像速度对比（`python tools\probe_mirrors.py` 可复跑）：

| 源 | 吞吐 |
|---|---|
| 直连 github.com | 176 KB/s |
| ghproxy.net | 116 KB/s |
| **gh-proxy.com** | **8.5 MB/s（最快，已作为首选）** |
| ghfast.top | 285 KB/s |
| gh.llkk.cc | 205 KB/s |

脚本会按 `gh-proxy.com → ghfast.top → ghproxy.net → 直连` 顺序自动降级，
每份文件都做 SHA 校验（WiX 用 SHA256，NSIS 用 SHA1），校验不过就换源。

> 工具链装好后，之后再执行 `npm run tauri build` 都不会再触发下载。

**跨平台交叉编译提示**：Tauri 不支持从 macOS 直接编 Windows 包（反之亦然）。
在对应系统上分别 `npm run tauri build` 即可；任务 JSON 是两个平台通用的（见第九节）。

> Windows 下 **NTFS / exFAT / FAT32 均可正常读写**。
> FAT32 单文件上限 4 GB，放 DCP / ProRes 请用 exFAT 或 NTFS。
>
> **运行依赖**：Tauri 程序需要 WebView2 运行时。Windows 10 22H2（带 Edge）已预装，
> 通常无需额外处理；若目标机缺失，用 NSIS 包安装时会自动引导安装。

---

## 七、断点续传规则（核心逻辑）

拷贝前对比源文件与目标同名文件，四种情况：

| 条件 | 动作 | 代码位置 |
|---|---|---|
| 目标文件不存在 | 完整拷贝 | `copy.rs` → `CopyMode::Fresh` |
| 目标存在，**大小不一致** | **断点续传**：从目标文件末尾继续写入 | `copy.rs` → `CopyMode::Resume` |
| 目标存在，大小一致，**内容哈希相同** | 跳过 | `scan.rs` 判定为 `Skip`，不进入拷贝 |
| 目标存在，大小一致，**内容哈希不同** | 覆盖 | `copy.rs` → `CopyMode::Overwrite` |

### ⚠️「跳过」不是免费的：追加一轮为什么只补新源（0.4.3）

上表第三行的**跳过**判定要靠内容哈希，而 `hash::files_identical` 会把
**源和目标各完整读一遍**。一句话：**「备份幂等」在结果上成立，在代价上不成立。**

0.4.2 及以前，「运行中往左栏拖入新素材」会排一轮追加任务，而那一轮**带着全部源重跑**。
于是首轮 1 TB 备完之后，追加轮要先空转读 ≈ 2 TB（源一份 + 刚写进目标的一份）才能碰到新素材；
这段时间中栏每个源都是 `enterTransfers()` 铺的占位行，全部显示「排队中」——
看上去就是「排队了，但一个文件都没拷」（用户就是这样报上来的）。

0.4.3 起，追加轮**只发本轮新加进来的源**：前端 `state.roundKeys` 记下上一轮发过的源，
`run(dryRun, "append")` 做差集（`src/main.js`）。回归测试
`src-tauri/tests/resume_rule.rs::t13_second_round_rereads_everything_already_copied`
把这个代价钉死：同一批数据，带全源的轮次预扫描要读 **20,971,520 B**（10 MiB 源 + 10 MiB 目标），
只带新源的轮次读 **0 B**。

> 代价上的取舍：上一轮**失败**的文件不会在追加轮自动重试。所以只要上一轮有失败，
> 界面会明确提示「追加一轮只补新加的源，需要重试请点『开始备份』」。
> 要走全量（逐项重扫、跳过已备完的），直接点「开始备份」。

顺带把这段时间的界面反馈也补齐了：预扫描阶段不再整列显示「排队中」，而是
当前扫到的那一行显示流动滑块 + 正在扫描的文件名，其余显示「预扫描中…」
（`?scan=1` 预览场景可以核对，见 3.1）。

#### ⚠️ 追加轮是「承接」不是「重开」（0.4.4）

0.4.3 把追加轮改成「只发新源」之后，又暴露出一个配套问题：追加轮当时仍按**新任务**处理 ——
`run()` 一进来就 `ui.resetResults()` 清空结果表，进度事件也只带本轮那几个源、把中栏整列替换掉。
于是「先备 A（看着它拷完、出了校验值）→ 运行中再加 B、C → 全部跑完」之后，
**A 的校验值和中栏那一行都不见了**，看起来就像「只校验了后加的两个」。

现在追加轮改成承接上一轮：

| 位置 | 行为 |
|---|---|
| 校验结果表 | **不清空**，新结果往后追加；计数（通过/失败/跳过）为**累计**值 |
| 中栏传输列表 | 上一轮已完成的行**原样保留**，只给新源补行（`mergeSourceRows()`） |
| 日志 | 第一轮仍是「开始备份任务」，追加轮单独标「追加一轮备份（只补 N 个新源…）」 |

要重开一张干净的结果表，点「开始备份」跑全量轮即可（全量轮照旧先清空）。

> 随手修掉的两个显示问题：① 后端日志时间戳是 **UTC**（`now_iso8601()` 带 `Z`），
> 前端却按字符串切片当本地时间显示，在 GMT+8 上日志里会同时出现 03:27 和 11:27 两种时刻，
> 看着像两天的记录 —— 现在统一转成浏览器本地时间。
> ② 后端发出的日志正文里不要用 `**加粗**` 这类 Markdown 记号（日志面板是纯文本，会原样显示星号）。

### 校验算法（默认 SHA-256）

「任务选项」里可选：**SHA-256**（默认）或 **xxHash64**。

| 算法 | 摘要长度 | 吞吐（本机实测） | 用途 |
|---|---|---|---|
| **SHA-256** | 64 位十六进制 | **1.76 GB/s**（1800 MiB/s） | 结果可以对外核对：`shasum -a 256 文件` / Windows `certutil -hashfile 文件 SHA256` |
| xxHash64 | 16 位十六进制 | 17.1 GB/s（17530 MiB/s） | 只作内部快速比对、不需要密码学强度时 |

> 实测方法：`cargo test --release -- --ignored hash_throughput --nocapture`（Windows 10 / 本机 CPU，
> SHA-256 走 x86 SHA-NI 硬件指令；Apple Silicon 走 ARMv8 加密扩展，量级相同）。
> SHA-256 大约是 xxHash64 的 1/10，但 1.76 GB/s 已经快过绝大多数磁盘 ——
> 校验阶段的瓶颈通常是「盘 + 要读源和目标两遍」，而不是哈希本身。

同一次任务里 **预扫描查重、续传前缀校验、最终全量校验** 都用同一种算法 —— 
避免出现「同一个文件在两个地方用不同算法」的解释负担；换算法只影响下一次开始的任务
（结果表里会明确写出本次用的是哪一种，表头也跟着变）。

> 老版本的任务 JSON 没有这个字段，加载时按 SHA-256 补齐（`#[serde(default)]`），不会报错。

### 校验阶段的进度是怎么上报的（0.4.5 修）

校验一个文件要读**源 + 目标两遍**，所以「当前文件」的总量是 `size × 2`。
这里的进度**必须在读取块的回调里发**（和拷贝阶段同一个套路），不能等一个文件读完再发：

- 2 GB 的素材按 64 MB/s 要读 60 秒以上。只在文件末尾发一次 → 界面静默一分钟，
  看起来就是「进度条不动了 / 卡死了」。
- 文件收尾那次上报**不能用节流窗口判定**，必须强制发：小文件几百毫秒能连着读完好几个，
  走节流的话它们的进度会被整段吞掉 —— 表现是「结果表在涨、进度条不动」。

两条约定各有一条回归测试钉住（`src-tauri/tests/verify_progress.rs`）：

```
t14_verify_reports_progress_while_reading_a_big_file   # 边读边发，存在中间态
t15_every_file_boundary_forces_a_progress_report       # 每个文件边界一条不缺
```

> 这两条测试**能把旧写法测红**（改回「循环末尾发一次」后：8 MiB 文件只上报 1 条而不是 5 条；
> 10 个小文件只上报 2 条、`files_done` 只覆盖 `{1, 10}` —— 中间 8 个文件一条进度都没有）。

界面上对应的三处读数（`src/ui.js::renderProgress`）：

| 位置 | 含义 |
|---|---|
| `1.03%` | **总**进度。总量几百 GB 时 1% 就是好几 GB，所以 10% 以下给两位小数，否则几十秒才动一下 |
| `6.88 GB / 667 GB` | 总字节数（源 + 目标累计） |
| `4/263 · 当前文件 60%（1.38 GB / 2.31 GB） · <路径>` | 当前文件自己的进度 —— 大任务上这才是「活着」的那个读数 |

**额外安全设计（可选开关「续传前校验已写入部分」，默认开启）**：
真正的断点续传最怕「上次中断在 4 MiB 块中间」，此时目标文件末尾若干 KB 是半截数据。
开启后，续传前先比对**源文件前 N 字节**与**目标文件已有内容**的哈希：
一致才追加，不一致则自动改为从头覆盖重写。几十 GB 素材建议保持开启。

**续传的物理保障**：每个文件写完（或中断）时都会 `flush + sync_all()`，
已写入的数据真实落盘。硬盘中途被拔掉、程序被强杀，下次运行都能从断点接上。

---

## 八、冲突询问模式的两种工作方式

界面「任务选项」里的 **冲突时弹窗询问**：

- **关闭（默认，推荐）**：完全按上表的断点续传规则自动判定。
  备份 10 TB 素材不会跳出上万个弹窗 —— 这也是断点续传规则存在的意义。
- **开启**：任何同名文件都弹出模态框，四个选项：
  ① 跳过此文件 ② 覆盖此文件 ③ 全部跳过 ④ 全部覆盖（③④ 为粘性选择，后续不再询问）。
  弹窗会显示源/目标大小差、判定理由和建议动作，阻塞流程直到用户选择。

无论哪种模式，**Dry Run** 都会把「将要拷贝 / 续传 / 跳过 / 覆盖」逐条列在结果列表里，
且不写入任何数据。

---

## 九、跨平台任务文件

`保存任务` / `加载任务` 使用同一份 JSON 格式：

```json
{
  "version": 1,
  "app": "CineBackup",
  "createdAt": "2026-09-18T06:51:31Z",
  "targetDir": "/Volumes/BACKUP/DCP_2026",
  "sources": [
    "/Volumes/CARD_A/A001",
    "/Volumes/CARD_A/A002.mxf"
  ],
  "options": {
    "askOnConflict": false,
    "quickScan": false,
    "resumePrefixCheck": true,
    "verifyAfterCopy": true
  }
}
```

- **磁盘文件系统信息不入 JSON** —— 每次加载任务时重新探测，因为同一路径在不同机器上
  挂载的可能是不同格式的盘。
- 加载时按当前平台规范化分隔符（`\` ↔ `/`），并逐个探测存在性；
  缺失/跨平台的路径会在日志里给出明确警告，而不是静默丢弃。

---

## 十、性能与内存

| 项目 | 取值 | 说明 |
|---|---|---|
| 拷贝块大小 | 4 MiB | `util::CHUNK_SIZE`，恒定内存占用 |
| 哈希块大小 | 4 MiB | 同上，`hash.rs` 复用同一常量 |
| 进度事件节流 | 120–150 ms | 避免高频事件拖慢 UI |
| 拷贝/哈希并发 | 串行 | 按需求：不做双盘并行，专注顺序大文件吞吐 |
| 校验读取量 | 2 × 数据量 | 源一遍 + 目标一遍，ETA 按此计算 |

内存占用与文件大小**无关**，只与块大小相关：常驻约 4 MiB（拷贝）+ 4 MiB（哈希）。

---

## 十一、常见问题

**Q：界面显示「文件系统：未知」？**
路径尚未挂载或盘符不存在时会这样。先确认磁盘已挂载，再点「选择目标文件夹」重新探测。

**Q：macOS 上往 NTFS 盘写入失败？**
原生驱动只读。装第三方 NTFS 驱动，或改用 exFAT 格式的盘。

**Q：拷贝速度比 Finder / 资源管理器慢？**
每个文件结束都会 `sync_all()` 强制落盘，这是断点续传正确性的代价。
顺序大文件场景下影响很小；如确需极致速度，可关闭「续传前校验已写入部分」。

**Q：FAT32 目标盘报错？**
FAT32 单文件上限 4 GB，DCP/ProRes 请换 exFAT 或 NTFS。

**Q：任务跑一半想停？**
点「取消任务」。已完成文件保持完好，当前文件停在哪个块都行，
下次运行会自动续传（这正是本工具的设计目标）。

**Q：能在备份完成后删除源文件吗？**
本工具**只读源、只写目标**，绝不删除任何源文件。

**Q：校验时底下的进度条好像不动？**
先看**速度**那一栏（它比百分比灵敏得多）：

- 总量几百 GB 时，总百分比 1% 就是好几 GB —— 667 GB 的校验里 1% ≈ 6.7 GB，
  按 64 MB/s 要**一分半**才跳一次。所以百分比本身动得慢是正常的，
  现在 10% 以下给两位小数，并且额外显示「当前文件」自己的百分比与字节数。
- 速度只有 ~11 MB/s：素材在**网络盘**上（实测某台 SMB 共享单流就是 11 MB/s，
  开两条流能到 21 MB/s）—— 瓶颈是链路不是软件，修网络比改代码划算得多。
- 速度 ~200 MB/s：本地盘，这就是物理账 —— 校验要读「源 + 目标」两遍，
  600 GB 的数据要读 1200 GB，一小时跑不完是预期。读盘量的完整账见第七节
  「『跳过』不是免费的」那一节。

⚠️ 一个容易误判的点：**单个大文件时左上角的文件计数会一直停在 `0/1`**
（要整个文件读完才 +1），看着像卡住。以**字节数 / 当前文件百分比 / 速度**为准。

---

## 十二、明确不实现（按需求）

MHL 文件生成、双盘同时并行备份、磁盘挂载监听、自动弹出硬盘、代码签名公证、多语言。

---

## 十三、发布安装包到 GitHub Release

当前线上版本：**https://github.com/52ting/CineBackup/releases/tag/v0.4.4**
（无需登录即可下载：`https://github.com/52ting/CineBackup/releases/latest`）

### 13.1 一条命令发布

三样产物都准备好之后（Windows 走第 6 节、dmg 走第 5.3 节、源码包见下），

```bash
python tools/make_src_zip.py          # 生成 cinebackup-<版本>-src.zip（版本号读 package.json）
python tools/gh_release.py publish    # 建/复用 Release 并上传全部产物
```

脚本会按 `package.json` 的版本号建 tag `v<版本>`，Release 名 `CineBackup <版本>`，
发布说明自动取「上一个 tag → HEAD」的提交记录拼出来。

```bash
python tools/gh_release.py status           # 看所有 Release 与资产
python tools/gh_release.py verify           # 逐字节核对本地产物 vs 远端资产（发布后顺手跑）
python tools/gh_release.py publish 文件...   # 只传指定文件
python tools/gh_release.py publish --draft   # 先建草稿，自己看一眼再公开
```

默认上传这四个（缺哪个会直接报错，不会静默跳过）：

| 文件 | 来源 |
|---|---|
| `CineBackup_<版本>_x64-setup.exe` | Windows 第 6 节 `tauri build`（NSIS，推荐给用户） |
| `CineBackup_<版本>_x64_en-US.msi` | 同上（MSI，企业批量部署） |
| `CineBackup_<版本>_universal.dmg` | 第 5.3 节 GitHub Actions 产物，`watch_ci.py` 会自动下载到 `cinebackup-builds/` |
| `cinebackup-<版本>-src.zip` | `python tools/make_src_zip.py` |

**幂等**：已存在且大小一致的同名资产会跳过；大小不同则先删旧的再传。重复执行不会污染 Release。

### 13.2 为什么不用 `gh` CLI 或 GitHub 连接器

- **本机没装 `gh`**（`gh: command not found`），也不想去装。
- **GitHub 连接器是 App 令牌，做不了这件事**：它的 `push_files` 要求把文件内容**内联进工具参数**，
  几 MB 的二进制走一遍上下文不可行；而且连接器根本没有「上传 Release 资产」这个能力。

所以脚本走 REST API：建 Release 打 `POST /repos/{owner}/{repo}/releases`，
传资产打 `POST https://uploads.github.com/repos/{owner}/{repo}/releases/{id}/assets?name=...`。
凭据复用本机 Git Credential Manager 里推送时存下的授权
（和 `tools/watch_ci.py` 同一套），**只在内存里用，不打印、不落盘**。

### 13.3 注意事项

- ✅ 仓库是**公开**的，Release 资产**无需登录即可下载**，链接可以直接发给用户：
  `https://github.com/52ting/CineBackup/releases/latest`（永远指向最新一版）。
  已实测：不带任何凭据请求四个资产的 `browser_download_url`，均返回 **HTTP 206**。
- 🔒 已开启 **secret scanning + push protection**（公开仓库的第一道闸）：推送里若含
  已知形态的凭据会被 GitHub 直接拦下。`dependabot_security_updates` 仍是关的，
  需要 Dependabot 告警时才去 Settings → Code security 打开。
- ⚠️ 公开之后 **git 历史对全网可见**，以后提交前留意别把密钥写进代码
  （本仓库转公开前已扫过全部 76 个已跟踪文件，无密钥、无私钥、无 `.env`）。
- ⚠️ 打 tag 会触发 `build-macos.yml`（**tag 推送不受 `paths-ignore` 影响**，一定会跑）。
  公开仓库下这不再花钱，只是多等约 9 分钟——所以发布时先等 Windows 产物齐了再建 Release，
  免得白跑一轮。
- ⚠️ `.github/workflows/build-macos.yml` 的 `paths-ignore` 已含 `**.md`、`tools/**`、
  `cinebackup-builds/**` —— CI 的构建步骤全是内联的、不调用 `tools/` 里的任何脚本，
  所以改工具不会重出包。**但如果以后让 CI 去调某个 `tools/` 脚本，记得把它从忽略列表里拿掉。**
- macOS 包未签名，用户首次打开需要「仍要打开」或
  `xattr -dr com.apple.quarantine /Applications/CineBackup.app`（Release 说明里已写明）。

---

## 十四、许可证

**MIT License** —— 全文见仓库根目录 [`LICENSE`](LICENSE)。
版权归 **52ting**（`Copyright (c) 2026 52ting`）。

### 你可以做什么

| 行为 | 是否需要额外许可 |
|---|---|
| 使用（个人、公司内部、商业用途都算） | 不需要 |
| 修改、二次开发、再分发 | 不需要 |
| 打包进你自己的产品一起卖 | 不需要 |
| 删掉界面上的名字换成自己的 | 不需要 |

**唯一的条件**：保留版权声明和许可证全文 —— 也就是别把 `LICENSE` 删掉、
别抹掉 `Copyright (c) 2026 52ting` 这一行。

**没有担保（No Warranty）**：软件按「现状」提供，作者不对任何后果负责。
对备份工具这点要特别说明：**重要素材请务必保留至少两份独立拷贝，并定期做恢复演练** ——
校验值一致只证明「拷过去的东西没坏」，不证明「这套备份策略本身可靠」。

### 第三方依赖

- **Tauri 生态本身是 MIT / Apache-2.0 双许可**
  （[官方声明](https://github.com/tauri-apps/tauri#licenses)），与本项目的 MIT 不冲突。
- Rust 侧直接依赖：`tauri`、`tauri-plugin-dialog`、`serde`、`serde_json`、`sha2`、`xxhash-rust`；
  前端：`@tauri-apps/api`、`@tauri-apps/plugin-dialog`、`vite`。全部是宽松许可。
- 本项目**未引入任何 GPL / AGPL 代码**，所以在你自己的产品里用这里的代码，不会被「传染」成必须开源。

### 想换许可证？

本仓库的提交全部由作者一人完成，**没有第三方贡献**，版权完整归作者所有 ——
所以需要时可以整体换成别的许可证（例如以后为商业版做双授权），不必征求任何人同意。
唯一要注意的是：**已经按 MIT 拿到的授权无法收回**，改许可证只对之后的新版本生效。

> ⚠️ 顺带说明：`package.json` 里的 `"private": true` 是 **npm 的发布保险**
> （防止误执行 `npm publish`），**与「仓库是否公开」无关**，保持不动。
