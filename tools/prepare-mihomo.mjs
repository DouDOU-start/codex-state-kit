import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, stdio: "inherit" });
  if (result.error) {
    console.error(result.error.message);
    process.exit(1);
  }
  process.exit(result.status ?? 1);
}

function targetArch() {
  // Tauri sets these variables for beforeBuildCommand. They describe the
  // artifact being built, which may differ from the Node process architecture
  // when an Intel build is made on Apple Silicon (or vice versa).
  const target = `${process.env.TAURI_ENV_ARCH ?? ""} ${process.env.TAURI_ENV_TARGET_TRIPLE ?? ""}`.toLowerCase();
  if (target.includes("universal")) {
    throw new Error("暂不支持 universal macOS 构建，请分别构建 arm64 和 x86_64 安装包");
  }
  if (target.includes("aarch64") || target.includes("arm64")) return "arm64";
  if (target.includes("x86_64") || target.includes("amd64")) return "amd64";
  return process.arch === "arm64" ? "arm64" : "amd64";
}

if (process.platform === "win32") {
  run("powershell.exe", [
    "-NoProfile",
    "-ExecutionPolicy",
    "Bypass",
    "-File",
    path.join(root, "tools/prepare-mihomo.ps1"),
  ]);
} else if (process.platform === "darwin") {
  run("bash", [path.join(root, "tools/prepare-mihomo.sh"), targetArch()]);
} else {
  console.error("当前只为 Windows 和 macOS 下载 Mihomo 内核");
  process.exit(1);
}
