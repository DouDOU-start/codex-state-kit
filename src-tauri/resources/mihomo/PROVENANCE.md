# 内置 Mihomo 内核来源

## 固定版本

| 项目 | 记录 |
| --- | --- |
| 上游项目 | [mihomo](https://github.com/MetaCubeX/mihomo) |
| 版本 | [v1.19.31](https://github.com/MetaCubeX/mihomo/releases/tag/v1.19.31) |
| Windows x64 压缩包 | `mihomo-windows-amd64-v1.19.31.zip` |
| Windows x64 SHA-256 | `38b2420799d9e7cde77ec1a19c7150dd17ca77f7fb82d9f62cb8763a307eee67` |
| macOS Apple Silicon 压缩包 | `mihomo-darwin-arm64-v1.19.31.gz` |
| macOS Apple Silicon SHA-256 | `d131f44b3deb2a8356f7ac75048ad67a10d53243323951c4f3cda7b672922963` |
| macOS Intel 压缩包 | `mihomo-darwin-amd64-v1.19.31.gz` |
| macOS Intel SHA-256 | `3546681ebef3415e5dcbe7210a61aa80748136e95e6552768fd883df345508ed` |
| Linux x64 压缩包 | `mihomo-linux-amd64-v1.19.31.gz` |
| Linux x64 SHA-256 | `d5e74bbddbdfff49a1aef7775bf5911da59f0d7196ed509a0ac914b3653dd5f1` |

可执行文件不放进 Git 仓库。打包时下载上游未修改的当前平台内核，校验 SHA-256 后打进安装包；每个安装包只包含当前系统的内核。

应用只使用绑定在 `127.0.0.1` 的 mixed 端口和带密钥的外部控制器，不启用 TUN，也不安装网络驱动。

## 许可证

- [LICENSE](LICENSE)：上游 GPL-3.0 许可证原文。
- [BUILD-INFO.txt](BUILD-INFO.txt)：内核版本与下载地址。

许可证保留上游原文。随安装包分发时应保留这些文件。

## 复现与更新

Windows 在仓库根目录通过 PowerShell 运行：

```powershell
./tools/prepare-mihomo.ps1
```

macOS 可按当前架构运行：

```sh
bash ./tools/prepare-mihomo.sh arm64  # Apple Silicon
bash ./tools/prepare-mihomo.sh amd64  # Intel
```

Linux x64 运行：

```sh
bash ./tools/prepare-mihomo-linux.sh
```

`tauri build` 和发布流程都会执行同一脚本，校验压缩包 SHA-256 后再打包。终端用户无需单独下载内核。升级时同步更新脚本中的版本、校验值与本文件。
