#!/usr/bin/env node
/**
 * 把版本号一次性写进所有需要它的地方。
 *
 *   node scripts/set-version.mjs 0.0.2
 *
 * 覆盖 5 处：
 *   1. package.json                   —— 顶层 version
 *   2. package-lock.json              —— 顶层 version + packages."" version
 *   3. src-tauri/tauri.conf.json      —— version（NSIS 安装包名与「关于」页都取自它）
 *   4. src-tauri/Cargo.toml           —— [package] version
 *   5. src-tauri/Cargo.lock           —— 本工程那一条 [[package]] 的 version
 *
 * 漏改任何一处都会让产物版本对不上，所以这里宁可多写一遍也不让调用方手改。
 * 幂等：值已经正确时不会改动文件（便于 CI 里判断"要不要提交"）。
 */
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const CRATE = "miside-wallpaper-engine";

/** semver，允许 `-rc.1` / `+build.7` 这类后缀。 */
const VERSION_RE = /^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/;

const version = (process.argv[2] ?? "").trim();

if (!version) {
  console.error("用法: node scripts/set-version.mjs <版本号>\n例如: node scripts/set-version.mjs 0.0.2");
  process.exit(2);
}
if (!VERSION_RE.test(version)) {
  console.error(`版本号格式不合法：${version}（应为 x.y.z，可带 -rc.1 这类后缀）`);
  process.exit(2);
}

const changed = [];
const unchanged = [];

function read(relative) {
  return readFileSync(join(root, relative), "utf8");
}

function write(relative, text) {
  writeFileSync(join(root, relative), text);
}

/** 写入并登记结果；内容没变就不落盘，保持 mtime 干净。 */
function commit(relative, before, after) {
  if (before === after) {
    unchanged.push(relative);
    return;
  }
  write(relative, after);
  changed.push(relative);
}

/**
 * 替换**第一处**匹配。
 *
 * package.json / tauri.conf.json / Cargo.toml 里的目标字段都是文件里第一次出现的那个，
 * 后面的同名键（依赖版本、rust-version 等）绝不能被误伤，所以这里只动 first match。
 */
function replaceFirst(text, pattern, replacement, label) {
  if (!pattern.test(text)) {
    throw new Error(`没找到需要替换的字段：${label}`);
  }
  return text.replace(pattern, replacement);
}

// ---- 1. package.json -------------------------------------------------------
{
  const file = "package.json";
  const before = read(file);
  const after = replaceFirst(
    before,
    /^(\s*"version"\s*:\s*")[^"]*(")/m,
    `$1${version}$2`,
    `${file} 顶层 version`,
  );
  commit(file, before, after);
}

// ---- 2. package-lock.json --------------------------------------------------
// npm 自己就是用 2 空格 JSON + 结尾换行写的，所以 JSON 往返不会改变格式。
{
  const file = "package-lock.json";
  const before = read(file);
  const lock = JSON.parse(before);
  lock.version = version;
  if (lock.packages && lock.packages[""]) {
    lock.packages[""].version = version;
  }
  const after = `${JSON.stringify(lock, null, 2)}\n`;
  commit(file, before, after);
}

// ---- 3. src-tauri/tauri.conf.json -----------------------------------------
{
  const file = "src-tauri/tauri.conf.json";
  const before = read(file);
  const after = replaceFirst(
    before,
    /^(\s*"version"\s*:\s*")[^"]*(")/m,
    `$1${version}$2`,
    `${file} version`,
  );
  commit(file, before, after);
}

// ---- 4. src-tauri/Cargo.toml ----------------------------------------------
// 必须锚定行首：依赖写成 `tauri-build = { version = "2", ... }`，行首不是 version。
{
  const file = "src-tauri/Cargo.toml";
  const before = read(file);
  const after = replaceFirst(
    before,
    /^(version\s*=\s*")[^"]*(")/m,
    `$1${version}$2`,
    `${file} [package] version`,
  );
  commit(file, before, after);
}

// ---- 5. src-tauri/Cargo.lock ----------------------------------------------
// 只改本工程那一条 [[package]]，其它 crate 的 version 一律不碰。
{
  const file = "src-tauri/Cargo.lock";
  const before = read(file);
  const block = new RegExp(
    `(\\[\\[package\\]\\]\\r?\\nname = "${CRATE}"\\r?\\nversion = ")[^"]*(")`,
  );
  const after = replaceFirst(before, block, `$1${version}$2`, `${file} 中 ${CRATE} 的 version`);
  commit(file, before, after);
}

// ---- 汇总 ------------------------------------------------------------------
console.log(`目标版本：${version}`);
for (const file of changed) console.log(`  已更新  ${file}`);
for (const file of unchanged) console.log(`  未变化  ${file}`);
