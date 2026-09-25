import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, stdio: "inherit" });
  if (result.error) {
    throw result.error;
  }
  if ((result.status ?? 1) !== 0) {
    process.exit(result.status ?? 1);
  }
}

function targetArch() {
  // These variables are supplied by Tauri hooks and describe the artifact,
  // rather than the host running Node. This matters for cross-compiled Mac
  // builds made on the other Apple architecture.
  const target = `${process.env.TAURI_ENV_ARCH ?? ""} ${process.env.TAURI_ENV_TARGET_TRIPLE ?? ""}`.toLowerCase();
  if (target.includes("universal")) {
    throw new Error("暂不支持 universal macOS 构建，请分别构建 arm64 和 x86_64 安装包");
  }
  if (target.includes("aarch64") || target.includes("arm64")) return "arm64";
  if (target.includes("x86_64") || target.includes("amd64")) return "amd64";
  return process.arch === "arm64" ? "arm64" : "amd64";
}

try {
  if (process.platform === "win32") {
    const powershellArgs = ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"];
    run("powershell.exe", [...powershellArgs, path.join(root, "tools/prepare-mihomo.ps1")]);
  } else if (process.platform === "darwin") {
    const arch = targetArch();
    run("bash", [path.join(root, "tools/prepare-mihomo.sh"), arch]);
  } else if (process.platform === "linux") {
    run("bash", [path.join(root, "tools/prepare-mihomo-linux.sh")]);
  } else {
    throw new Error("当前只为 Windows、macOS 和 Linux 下载 Mihomo 内核");
  }
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  process.exit(1);
}
