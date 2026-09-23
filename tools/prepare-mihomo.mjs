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

if (process.platform === "win32") {
  run("powershell.exe", [
    "-NoProfile",
    "-ExecutionPolicy",
    "Bypass",
    "-File",
    path.join(root, "tools/prepare-mihomo.ps1"),
  ]);
} else if (process.platform === "darwin") {
  const arch = process.arch === "arm64" ? "arm64" : "amd64";
  run("bash", [path.join(root, "tools/prepare-mihomo.sh"), arch]);
} else {
  console.error("当前只为 Windows 和 macOS 下载 Mihomo 内核");
  process.exit(1);
}
