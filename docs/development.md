# 开发与打包

[返回首页](../README.md)

## 环境

发布流程会按目标平台下载对应的 WARP 与 Mihomo 内核：Windows x64、macOS Intel 和 macOS Apple Silicon 分别使用对应二进制文件，不跨平台复用。

准备 Node.js、Rust stable、C++ 构建工具和 WebView2 等 [Tauri 开发依赖](https://v2.tauri.app/start/prerequisites/)。pnpm 版本以根目录 `package.json` 的 `packageManager` 为准；以下命令使用 Corepack 调用。

```sh
git clone git@github.com:DouDOU-start/codex-state-kit.git
cd codex-state-kit
corepack pnpm install --frozen-lockfile
corepack pnpm dev
```

若环境没有 Corepack，先准备 Corepack 或安装与项目声明一致的 pnpm。已安装对应 pnpm 时，可将命令中的 `corepack pnpm` 替换为 `pnpm`。

## 常用命令

均在仓库根目录执行：

| 命令 | 用途 |
| --- | --- |
| `corepack pnpm dev` | 启动桌面开发应用与前端服务 |
| `corepack pnpm dev:renderer` | 仅启动浏览器预览，默认端口 1420 |
| `corepack pnpm typecheck` | TypeScript 类型检查 |
| `corepack pnpm build:renderer` | 类型检查并构建前端到 `dist/` |
| `cargo check --workspace` | 检查 Rust 工作区编译 |
| `cargo test -p codex-state-kit login` | 登录及相关接入测试 |
| `corepack pnpm icons` | 从统一 SVG 生成应用图标 |
| `corepack pnpm build` | 在当前系统构建桌面程序及安装包 |

纯浏览器预览使用模拟接口，不会修改真实 Codex 配置或建立 WARP 隧道。桌面开发版会执行真实操作，建议使用专门的 Codex 配置目录。

当前部分 Token 缓存测试会读写用户目录，且测试之间存在共享文件影响。完整测试应在隔离、可丢弃的用户环境执行；不要直接在日常账号目录运行全量测试。登录测试使用临时目录和本机模拟服务。

## GitHub Actions 发布

推送形如 `v0.0.1` 的标签会触发 `.github/workflows/release.yml`，构建 Windows x64、macOS Intel 和 macOS Apple Silicon，并将安装包、更新包与签名上传到同一个草稿 Release。构建任务依次执行，避免并发覆盖 `latest.json`；所有平台成功且更新清单验证通过后才公开发布。也可以在 Actions 页面手动运行工作流。已公开版本不允许覆盖，需提高版本号。

发布说明来自**附注标签**的正文，不要用轻量标签。附注里不要写以 `#` 开头的行（Git 会当成注释丢掉），小标题直接写「功能」「本次更新」即可。PowerShell 示例：

```powershell
git tag -a v0.0.4 -m @"
本次更新：
- 说明一
- 说明二

Mac 版本当前为未公证构建；首次打开时请按系统提示允许应用运行。
"@
git push origin v0.0.4
```

在 Actions 里手动跑工作流时，也可以在 `notes` 输入框填写说明。已经生成的 Release 仍可在 GitHub 上点 Edit 改说明。未写附注时才回落到默认文案。

当前工作流生成未签名、未公证的 Mac 包。正式分发前，在仓库 Secrets 配置 Apple Developer 证书和公证凭据，并在工作流中接入 Tauri 的签名环境变量；否则 macOS 可能显示安全提示。

## 版本与产物

首个版本为 `0.0.1`。发布时修改根目录 `package.json` 的 `version`，不带 `v` 前缀；标题栏与 Tauri 安装包读取同一值。同步维护两个 `Cargo.toml` 中的包版本，并更新 `Cargo.lock`。

### 应用内更新检查

正式版启动时及运行期间每 6 小时查询本仓库 GitHub Releases 的最新正式版，使用 SemVer 比较本机安装包版本和发布标签（支持 `v` 前缀）。草稿和预发布不会触发提示；本机版本相同或更高也不会提示升级。

标题栏「检查更新」可手动触发，成功结果缓存 60 秒，并合并同时发起的请求。后台检查失败保持安静；手动检查会显示网络、限流等错误，并提供发布页面入口。「稍后」关闭本次运行中该版本的提醒，手动检查可以重新显示。开发版只支持手动检查，浏览器预览不调用真实更新接口。

检查请求使用应用默认网络，不使用 Token 获取代理或专用于业务转发的上游代理。网络不通时可以直接在浏览器访问发布页面。下载按钮由后端根据合法版本标签生成本仓库发布链接，不接受任意外部地址。

已接入 [Tauri Updater](https://v2.tauri.app/plugin/updater/)：用户点击「下载更新」后后台下载并校验签名，完成后点击「确认安装并重启」。安装前停止后台巡检，恢复 Codex 路由、停止 WARP 并等待在途请求结束。安装失败会恢复服务；不强制静默重启。更新清单尚不存在、当前平台包缺失或签名校验失败时，可继续使用原版本或跳转发布页。配置与签名维护见[自动更新发布](updater.md)。

构建成功后查看：

```text
target/release/codex-state-kit-desktop.exe
target/release/bundle/nsis/
target/release/bundle/msi/
```

构建前会下载并校验当前目标平台的 Mihomo 内核，再重新生成图标与前端资源；内核会随 Tauri 安装包放进应用 Resources，运行时不再下载。macOS Apple Silicon 与 Intel 安装包分别内置对应架构的二进制，不能交叉复用。内核二进制不提交到仓库。优先分发安装包；单独分发主程序时必须附带 `warp/` 与 `mihomo/` 资源目录。内核更新步骤见 [WARP 来源记录](../src-tauri/resources/warp/PROVENANCE.md) 与 [Mihomo 来源记录](../src-tauri/resources/mihomo/PROVENANCE.md)。

## 代码结构

| 路径 | 职责 |
| --- | --- |
| `frontend/src/` | React 界面、样式与模拟接口 |
| `src-tauri/src/` | 桌面生命周期、IPC 命令与登录会话 |
| `src/proxy.rs` | HTTP SSE 转发、获取调度与自动接入管理；WebSocket 返回 426 触发客户端回退 |
| `src/attach.rs` | Codex 配置修改、备份与恢复 |
| `src/login.rs`、`src/browser_login.rs` | 授权码登录、回调登录与凭据保存 |
| `src/fetch.rs`、`src/turn_state.rs` | Turn-State 获取、缓存与替换 |
| `src/warp.rs` | 内置内核生命周期与健康检查 |
| `tools/` | 内核下载和依赖声明维护 |

发布前执行适用检查，再按[维护与验收](turn-state-sop.md)验证桌面行为。协议、默认值或文件路径变化时同步更新文档；未接入运行流程的辅助函数不作为已支持功能。
