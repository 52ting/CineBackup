# CineBackup 装到 Mac —— 操作指南

这份指南假设你手上是一台 **Apple Silicon 的 Mac**，要把它装到自己和别人的 Mac 上。
全程只要在「终端」里复制粘贴，不需要懂编程。

> 为什么要在 Mac 上折腾？因为 Mac 的安装包没法在 Windows 上生成 —— 这是 Tauri
> 的硬限制，跟这个项目无关。源码已经在同一个 zip 里了，照着下面做就行。

---

## 第 1 步 · 把源码弄到 Mac

zip 文件：`cinebackup-0.4.0-src.zip`（1.15 MB）

任选一种传输方式：

- 用微信「文件传输助手」发给自己 → 在 Mac 端微信下载
- 拷进 U 盘 → 插到 Mac
- 走网盘（iCloud / OneDrive / 百度网盘）中转

然后在 Mac 上**双击解压**，会得到一个 `cinebackup` 文件夹。
把它拖到你的用户主目录下，变成 `~/cinebackup`（放桌面也行，路径别带奇怪符号就好）。

> 如果双击解压出来是一堆散落文件，说明 Mac 自动解压到了一个同名文件夹里，正常。

---

## 第 2 步 · 打开终端

按 `Cmd + 空格`，输入 `终端`（或 `Terminal`），回车。

然后进到项目目录、确认解压正确：

```bash
cd ~/cinebackup
ls
```

`ls` 的输出里应该能看到 `README.md`、`tools`、`src`、`src-tauri` 这几项。
看到了就说明位置对了；没有的话，用 `cd ` 加上你实际解压的路径。

---

## 第 3 步 · 装三样依赖（每台 Mac 只需一次）

### ① Xcode 命令行工具 —— 提供编译用的 linker / SDK

```bash
xcode-select --install
```

会弹一个窗，点「安装」，等它下载完（几百 MB，几分钟）。**必须等它结束再做下一步。**

### ② Rust —— 编译后端

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

中途会问你怎么装，直接**回车**选默认那一项（`1) Proceed with standard installation`）。

装完让当前这个终端窗口认识它：

```bash
. "$HOME/.cargo/env"
```

### ③ Node.js —— 构建前端界面

```bash
brew install node
```

如果提示 `brew: command not found`，说明没装 Homebrew，先装它（一行，会要求输密码）：

```bash
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

### 验证四条命令都能打出版本号

```bash
xcode-select -p
cargo -V
node -v
npm -v
```

四条都有输出，就齐了。中间任何一条报 `command not found`，说明对应的那一步没成功，重做。

---

## 第 4 步 · 打包

```bash
cd ~/cinebackup
UNIVERSAL=1 bash tools/build_macos.sh
```

`UNIVERSAL=1` = 打**通用二进制**，Intel 和 Apple Silicon 的 Mac 都能跑，方便分给别人。
（只装自己这台的话，去掉 `UNIVERSAL=1` 能快不少、包也小一半。）

过程说明：

- 先 `npm install` 下前端依赖，然后开始编译
- **首次编译要 3–10 分钟**，中途要联网下几百个 Rust 依赖包，进度条看着像卡住是正常的
- 结束时脚本会把产物路径和后续命令直接打印出来

产物在这里：

```
src-tauri/target/universal-apple-darwin/release/bundle/
├── dmg/CineBackup_0.4.0_universal.dmg      ← 分发用这个
└── macos/CineBackup.app                    ← 也可以直接拖进「应用程序」
```

---

## 第 5 步 · 装到自己这台 Mac

**方式 A**（命令行，三步）：

```bash
cd ~/cinebackup
cp -R "src-tauri/target/universal-apple-darwin/release/bundle/macos/CineBackup.app" /Applications/
xattr -dr com.apple.quarantine /Applications/CineBackup.app
open /Applications/CineBackup.app
```

**方式 B**（图形界面）：双击 `bundle/dmg/` 里那个 `.dmg`，把 CineBackup 图标拖进「应用程序」，
然后回终端补跑一次：

```bash
xattr -dr com.apple.quarantine /Applications/CineBackup.app
```

> ⚠️ 那条 `xattr` 命令**不能省**。不做这一步，打开时会提示
> 「CineBackup 已损坏，无法打开」—— 它没坏，只是没有 Apple 的签名，
> 被系统的 Gatekeeper 拦下了。`xattr` 就是把它下载时带的"隔离"标记去掉。

---

## 第 6 步 · 装到别人的 Mac

把 `CineBackup_0.4.0_universal.dmg` 发给对方（微信 / U 盘 / 网盘都行）。

对方要做三件事：

1. 双击 dmg，把 CineBackup 拖进「应用程序」
2. 打开「终端」，粘贴这一行并回车：
   ```bash
   xattr -dr com.apple.quarantine /Applications/CineBackup.app
   ```
3. 双击打开

> 想彻底省掉这一步，得去买 Apple 开发者账号（$99/年）做签名 + 公证。
> 自己用的话，一条命令比这划算得多。
>
> 顺带提醒：对方如果用的是 Intel Mac，通用二进制也能跑 —— 这正是选择通用版的原因。

---

## 第 7 步 · 第一次使用

界面是三栏的：

- **左栏**：拖入要备份的素材（从访达拖过来就行），或点「＋」手动选
- **中栏**：本机磁盘。点一块盘会弹出菜单 →「设为目标」
- **右栏**：确认目标路径和剩余空间
- 点「开始备份」（想先预演就点「Dry Run」，它不写任何数据）

开始后中栏会自动折叠，换成**每个源一行**的进度条，带当前文件名、速度和预计剩余时间。

### 这几个现象是正常的，别当成故障

| 现象 | 原因 |
|---|---|
| 「系统盘」卡片显示「只读」 | macOS 11 之后 `/` 本来就是只读系统卷 |
| 拖「桌面 / 文档 / 下载」里的文件时弹授权框 | 系统的隐私保护（TCC），点「允许」即可 |
| NTFS 目标盘被标成只读、不让选 | macOS 原生驱动只读，要写入得装 Paragon / Tuxera / Mounty |
| 备份出来的目录很干净，没有 `.DS_Store` / `._*` | 遍历时按 macOS 规则过滤掉了（Windows 版不做这个过滤） |
| 备份速度比访达慢一点 | 每个文件写完都强制落盘，这是断点续传正确性的代价 |

---

## 报错对照表

| 报错 | 原因 / 解法 |
|---|---|
| `xcrun: error: invalid active developer path` | 第 3 步的 `xcode-select --install` 没装完，重跑并等它彻底结束 |
| `cargo: command not found` | 没执行 `. "$HOME/.cargo/env"`，或者重开一个终端窗口再试 |
| `brew: command not found` | 先装 Homebrew（见第 3 步 ③ 的注释） |
| `error: linker 'cc' not found` | Xcode 命令行工具没装好，重跑 `xcode-select --install` |
| 长时间没反应 / 卡在下载 | 在拉 Rust 依赖，网络慢（国内常见）。等等看，或换个网络重跑 |
| 打开时提示「已损坏」 | 忘了跑 `xattr -dr com.apple.quarantine ...`，见第 5 步 |
| `Permission denied` 跑脚本 | 用 `bash tools/build_macos.sh` 调用，别直接 `./tools/build_macos.sh` |
| 编译时风扇狂转、很慢 | 正常，release 模式开了 LTO 优化，第一次最慢 |
| 提示脚本只能在 macOS 上跑 | 你在 Windows / WSL 里执行了它 —— 必须真的在 Mac 上跑 |

---

## 附 · 不想在 Mac 上编译？

把这份源码推到 GitHub，仓库里已经准备好了云端打包配置
（`.github/workflows/build-macos.yml`）：

1. 在 GitHub 仓库页面点 **Actions** 标签
2. 左边选 **build-macos** → 右边点 **Run workflow**
3. 等几分钟，在本次运行的 **Artifacts** 里下载 `CineBackup-macOS-universal`

云端 Mac 打出来的包和本地一样，照样要跑那条 `xattr` 命令。本地什么都不用装。
