#!/usr/bin/env node
/**
 * 在 Visual Studio 的 x64 原生工具环境下执行命令。
 *
 *   node scripts/with-msvc.mjs tauri dev
 *   node scripts/with-msvc.mjs cargo build
 *
 * 背景：本机的 Visual Studio 生成工具装在非默认目录（D:\VSBuildTools），
 * 且没有注册到 rustc 能自动发现的位置，于是 rustc 会报
 * `linker link.exe not found`。这里手动定位并导入 vcvars64.bat 设置的环境变量，
 * 让 cargo / rustc 能找到 link.exe 与 MSVC 的 CRT 库。
 *
 * 注意：必须在临时 .cmd 文件里执行 `call vcvars64.bat`，
 * 用 `cmd /c "call xxx.bat & set"` 这种单行写法会让 vcvars 静默失效。
 */
import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** 从 VS 安装器的实例清单里读出安装路径（最可靠，不依赖注册表）。 */
function installPathsFromInstances() {
  const base = "C:\\ProgramData\\Microsoft\\VisualStudio\\Packages\\_Instances";
  const paths = [];
  if (!existsSync(base)) return paths;

  for (const name of readdirSync(base)) {
    const statePath = join(base, name, "state.json");
    if (!existsSync(statePath)) continue;
    let text;
    try {
      text = readFileSync(statePath, "utf8");
    } catch {
      continue;
    }
    // state.json 带 BOM，用正则取字段比 JSON.parse 稳。
    const match = /"installationPath"\s*:\s*"((?:[^"\\]|\\.)*)"/.exec(text);
    if (match) paths.push(match[1].replace(/\\\\/g, "\\"));
  }
  return paths;
}

/** 退路：调用 vswhere.exe 查询。 */
function installPathsFromVswhere() {
  const vswhere = "C:\\Program Files (x86)\\Microsoft Visual Studio\\Installer\\vswhere.exe";
  if (!existsSync(vswhere)) return [];

  const result = spawnSync(vswhere, ["-all", "-products", "*", "-property", "installationPath"], {
    encoding: "utf8",
  });
  if (result.status !== 0 || !result.stdout) return [];
  return result.stdout
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
}

function findVcvars() {
  const roots = [
    process.env.VSINSTALLDIR,
    ...installPathsFromVswhere(),
    ...installPathsFromInstances(),
    "C:\\Program Files\\Microsoft Visual Studio\\2022\\BuildTools",
    "C:\\Program Files\\Microsoft Visual Studio\\2022\\Community",
    "C:\\Program Files\\Microsoft Visual Studio\\2022\\Professional",
    "C:\\Program Files\\Microsoft Visual Studio\\2022\\Enterprise",
    "C:\\Program Files (x86)\\Microsoft Visual Studio\\2022\\BuildTools",
  ].filter(Boolean);

  for (const root of roots) {
    for (const name of ["vcvars64.bat", "vcvarsall.bat"]) {
      const candidate = join(root, "VC", "Auxiliary", "Build", name);
      if (existsSync(candidate)) return candidate;
    }
  }
  return null;
}

/** 通过临时 .cmd 执行 vcvars 并回吐环境变量，再合并进当前进程。 */
function importMsvcEnv(batPath) {
  const wrapper = join(tmpdir(), `msvc-env-${process.pid}.cmd`);
  const script = [
    "@echo off",
    "chcp 65001 >nul",
    `call "${batPath}" >nul 2>&1`,
    "set",
    "",
  ].join("\r\n");
  writeFileSync(wrapper, script, "utf8");

  let content = "";
  try {
    const result = spawnSync("cmd.exe", ["/d", "/c", wrapper], {
      encoding: "utf8",
      maxBuffer: 32 * 1024 * 1024,
    });
    content = result.stdout ?? "";
    if (!content.trim()) {
      throw new Error(`执行 vcvars 失败：${batPath}\n${result.stderr ?? ""}${result.error ?? ""}`);
    }
  } finally {
    try {
      unlinkSync(wrapper);
    } catch {
      /* 忽略清理失败 */
    }
  }

  let count = 0;
  for (const line of content.split(/\r?\n/)) {
    const eq = line.indexOf("=");
    if (eq <= 0) continue;
    const key = line.slice(0, eq);
    process.env[key] = line.slice(eq + 1);
    count += 1;
  }
  return count;
}

function main() {
  const args = process.argv.slice(2);
  if (args.length === 0) {
    console.error(
      "用法: node scripts/with-msvc.mjs <命令> [参数...]\n" +
        "例如: node scripts/with-msvc.mjs tauri dev",
    );
    process.exit(2);
  }

  if (process.platform === "win32") {
    const vcvars = findVcvars();
    if (vcvars) {
      const count = importMsvcEnv(vcvars);
      console.log(`[with-msvc] 已导入 ${count} 个环境变量 ← ${vcvars}`);
    } else {
      console.warn(
        "[with-msvc] 未找到 vcvars64.bat，直接执行命令（若已安装 MSVC 并配置好环境可忽略）",
      );
    }
  }

  const [command, ...rest] = args;

  // 直接 `node scripts/with-msvc.mjs tauri dev` 时 node_modules/.bin 不在 PATH 上，
  // 这里手动解析一下，让 npm 脚本和直接调用都能工作。
  let executable = command;
  const binDir = join(process.cwd(), "node_modules", ".bin");
  for (const name of [`${command}.cmd`, `${command}.exe`, command]) {
    const candidate = join(binDir, name);
    if (existsSync(candidate)) {
      executable = candidate;
      break;
    }
  }

  const result = spawnSync(executable, rest, {
    stdio: "inherit",
    env: process.env,
    shell: process.platform === "win32",
  });

  if (result.error) {
    console.error(`[with-msvc] 启动失败: ${result.error.message}`);
    process.exit(1);
  }
  process.exit(result.status ?? 0);
}

main();
