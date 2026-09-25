# 自动更新发布

[返回首页](../README.md)

应用使用 Tauri updater 下载并验证签名。Windows 使用 NSIS 更新包，Linux 使用签名的 `.AppImage`，macOS 使用 `.app.tar.gz`；下载期间不停止转发，用户确认后才安装重启。Windows 使用 `passive` 安装模式，显示安装进度；系统权限提示仍可能出现。

## 签名与 Secrets

本项目的公钥位于 `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey`，`bundle.createUpdaterArtifacts` 已启用。私钥不在仓库中，本机默认保存在：

```text
%USERPROFILE%\.tauri\codex-state-kit\updater.key
%USERPROFILE%\.tauri\codex-state-kit\updater.key.pub
%USERPROFILE%\.tauri\codex-state-kit\updater.password
```

该目录仅授权当前 Windows 用户与 SYSTEM 访问。请另外安全备份私钥和密码；不要重新生成或替换密钥，否则已安装版本无法验证之后的更新包。公开公钥不需要保密。

在有仓库管理权限的 GitHub 账号完成 CLI 登录后执行：

```powershell
./tools/set-updater-secrets.ps1
```

脚本核对本机公钥与应用配置一致，将以下两个值通过标准输入交给 GitHub CLI，由 CLI 加密写入 `DouDOU-start/codex-state-kit` 的 Actions Secrets，不在控制台输出密钥或密码：

- `TAURI_SIGNING_PRIVATE_KEY`
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`

脚本优先使用 PATH 中的 `gh`，也支持本机 `%LOCALAPPDATA%/codex-state-kit-tools/gh/bin/gh.exe`。首次配置需要 GitHub 浏览器授权，SSH 推送权限不能代替 Secrets API 权限。

## 发布新版本

1. 同步提高 `package.json`、两个 `Cargo.toml` 的版本号，并更新 `Cargo.lock`。旧版 `0.0.4` 用户需手动安装一次带 updater 公钥的新版本，之后才支持应用内更新。
2. 提交代码并推送相同版本的 `vX.Y.Z` 标签。发布流程首先检查版本号和两个 Secrets 是否存在；已公开的版本禁止覆盖。
3. 四个平台依次构建签名更新包，上传到草稿 Release，并合并生成 `latest.json`。Windows 清单优先使用 NSIS `.exe`，Linux 清单使用签名 AppImage 更新包。
4. 最终任务核对四个平台条目、版本、下载地址及安装包和 `.sig` 资源齐全，再公开 Release 并标记 latest。构建失败时草稿保持不公开，可修复后重新运行。

固定更新入口：`https://github.com/DouDOU-start/codex-state-kit/releases/latest/download/latest.json`。更新包 URL 指向对应版本标签，避免下载时混用不同版本的资源。

## 本地验证

```powershell
cargo test -p codex-state-kit update::tests
node --test tools/verify-updater-manifest.test.mjs
corepack pnpm build:renderer
cargo check --workspace
```

签名测试使用无执行能力的固定文本和公开签名，验证应用公钥能验签，并拒绝篡改内容。构建真实签名安装包时，将 `TAURI_SIGNING_PRIVATE_KEY` 设置为私钥路径，`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 设置为密码文件内容，再运行 `corepack pnpm build`；不要将值写进提交、日志或截图。

首次发布后仍需在 Windows 与两种 macOS 安装环境中验证从旧安装包到新安装包的完整升级，包括签名失败、网络中断、安装失败恢复、路由与 `auth.json` 恢复、订阅内核退出和重启后的版本号。Tauri updater 签名与操作系统代码签名/公证是不同机制；此配置不改变现有 macOS 公证状态。
