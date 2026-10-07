//! 壁纸库：zip 导入（GBK 文件名回退 + zip-slip 防护）、索引读写、列表、删除、改名。
//!
//! 目录约定（见 `docs/api-contract.md` 第 6 节）：
//! * 索引：`<app_config_dir>/wallpapers.json`
//! * 单个壁纸：`<libraryRoot>/<id>/`
//!
//! 导入是个重活（几个 GB 的解压），全部在调用方的阻塞线程里做，边做边把进度 emit 给界面。

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime};

use crate::prefs::Settings;

/// 壁纸库列表变化事件。
pub const LIBRARY_EVENT: &str = "wp://library";
/// 导入进度事件。
pub const IMPORT_PROGRESS_EVENT: &str = "wp://import-progress";

/// 进度事件的最小间隔：解压一个几万文件的包时，不节流会把 WebView 淹掉。
const PROGRESS_INTERVAL_MS: u128 = 100;
/// 解压总量上限（防止 zip 炸弹把盘写满）。
const MAX_TOTAL_BYTES: u64 = 24 * 1024 * 1024 * 1024;
/// 条目数上限。
const MAX_ENTRIES: usize = 200_000;
/// 寻找主程序时的最大目录深度。
const EXE_SEARCH_DEPTH: usize = 4;

/// 单个壁纸（契约 2.1）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WallpaperEntry {
    pub id: String,
    pub name: String,
    pub dir: String,
    pub exe: String,
    pub exe_name: String,
    pub size_bytes: u64,
    pub file_count: u64,
    pub imported_at: String,
    pub unity: bool,
    pub unity_version: String,
    pub broken: bool,
    pub missing_exe: bool,
}

impl WallpaperEntry {
    /// 按当前磁盘情况刷新 `broken` / `missingExe`（索引里存的是导入那一刻的结果）。
    pub fn revalidated(mut self) -> Self {
        let dir = PathBuf::from(&self.dir);
        let exe_exists = !self.exe.is_empty() && Path::new(&self.exe).is_file();
        self.broken = !dir.is_dir();
        self.missing_exe = !exe_exists;
        self
    }

    /// 能不能启动：目录在、exe 在。
    pub fn is_runnable(&self) -> bool {
        !self.broken && !self.missing_exe
    }
}

/// 导入进度负载（契约第 4 节）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProgress {
    /// `reading` / `extracting` / `finalizing` / `done`
    pub phase: String,
    pub percent: f32,
    pub files: u64,
    pub total_files: u64,
    pub current: String,
    pub id: String,
}

/// 落盘的索引文件。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Index {
    entries: Vec<WallpaperEntry>,
}

/* ------------------------------------------------------------------ 索引 */

fn config_dir<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
}

fn index_file<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    config_dir(app).join("wallpapers.json")
}

fn load_index<R: Runtime>(app: &AppHandle<R>) -> Index {
    fs::read_to_string(index_file(app))
        .ok()
        .and_then(|text| serde_json::from_str::<Index>(crate::prefs::strip_bom(&text)).ok())
        .unwrap_or_default()
}

/// 先写临时文件再替换：中途断电也不会把索引写坏。
fn save_index<R: Runtime>(app: &AppHandle<R>, index: &Index) -> Result<(), String> {
    let file = index_file(app);
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
    }
    let text = serde_json::to_string_pretty(index).map_err(|e| format!("序列化索引失败：{e}"))?;
    let temp = file.with_extension("json.tmp");
    fs::write(&temp, text).map_err(|e| format!("写入 {} 失败：{e}", temp.display()))?;
    fs::rename(&temp, &file).map_err(|e| format!("替换 {} 失败：{e}", file.display()))
}

/// 列表：按导入时间倒序，并把每条按磁盘现状重新校验一遍。
pub fn list<R: Runtime>(app: &AppHandle<R>, _settings: &Settings) -> Vec<WallpaperEntry> {
    let mut entries: Vec<WallpaperEntry> = load_index(app)
        .entries
        .into_iter()
        .map(WallpaperEntry::revalidated)
        .collect();
    entries.sort_by(|a, b| b.imported_at.cmp(&a.imported_at));
    entries
}

/// 按 id 取一条。
pub fn find<R: Runtime>(
    app: &AppHandle<R>,
    id: &str,
) -> Result<WallpaperEntry, String> {
    load_index(app)
        .entries
        .into_iter()
        .find(|entry| entry.id == id)
        .map(WallpaperEntry::revalidated)
        .ok_or_else(|| format!("找不到壁纸：{id}"))
}

/// 条目数量与总占用（给「关于」页用）。
pub fn stats<R: Runtime>(app: &AppHandle<R>) -> (usize, u64) {
    let entries = load_index(app).entries;
    let total = entries.iter().map(|entry| entry.size_bytes).sum();
    (entries.len(), total)
}

/// 广播列表。
pub fn broadcast<R: Runtime>(app: &AppHandle<R>, entries: &[WallpaperEntry]) {
    let _ = app.emit(LIBRARY_EVENT, entries.to_vec());
}

/* ------------------------------------------------------------------ 导入 */

/// 解压统计。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ExtractReport {
    /// 实际写出的文件数。
    pub files: u64,
    /// 实际写出的字节数。
    pub bytes: u64,
    /// 被跳过的条目数（目录、符号链接、不安全路径）。
    pub skipped: u64,
}

/// 目录体检结果。
#[derive(Debug, Clone, Default)]
pub struct InspectResult {
    pub exe: Option<PathBuf>,
    pub unity: bool,
    pub unity_version: String,
    pub size_bytes: u64,
    pub file_count: u64,
}

/// 把 zip 解压到 `target_dir`（目录要已存在且为空）。
///
/// 进度回调签名：`(phase, percent, files, total_files, current)`；`phase` 为
/// `reading` / `extracting`。抽成独立函数是为了能脱离 Tauri 直接单测 ——
/// 解压 + 认主程序是整条导入链里最容易出错的一段。
pub fn extract_zip(
    zip_path: &Path,
    target_dir: &Path,
    mut on_progress: impl FnMut(&str, f32, u64, u64, &str),
) -> Result<ExtractReport, String> {
    let file = File::open(zip_path)
        .map_err(|e| format!("打开压缩包 {} 失败：{e}", zip_path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("这不是一个能读的 zip：{e}"))?;

    on_progress("reading", 0.0, 0, archive.len() as u64, "");

    // 先把清单读出来：总量用于算百分比，也顺便做炸弹防护
    let mut plan: Vec<(usize, String, PathBuf, u64)> = Vec::with_capacity(archive.len());
    let mut total_bytes: u64 = 0;
    let mut total_files: u64 = 0;
    let mut skipped: u64 = 0;

    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|e| format!("读取压缩包第 {index} 项失败：{e}"))?;
        if entry.is_dir() {
            continue;
        }
        if entry.is_symlink() {
            // 符号链接一律不落地：解压出来可能指向库外的任意路径
            skipped += 1;
            continue;
        }
        let raw_name = entry.name_raw().to_vec();
        let decoded = decode_entry_name(&raw_name);
        let Some(relative) = safe_relative_path(&decoded) else {
            skipped += 1;
            continue;
        };
        total_bytes = total_bytes.saturating_add(entry.size());
        total_files += 1;
        if total_bytes > MAX_TOTAL_BYTES {
            let _ = fs::remove_dir_all(target_dir);
            return Err(format!(
                "压缩包解压后超过 {} GB，已拒绝导入",
                MAX_TOTAL_BYTES / 1024 / 1024 / 1024
            ));
        }
        if plan.len() >= MAX_ENTRIES {
            let _ = fs::remove_dir_all(target_dir);
            return Err(format!("压缩包条目超过 {MAX_ENTRIES} 个，已拒绝导入"));
        }
        plan.push((index, decoded, relative, entry.size()));
    }

    let mut report = ExtractReport {
        skipped,
        ..Default::default()
    };
    let mut buffer = vec![0u8; 256 * 1024];

    for (index, decoded, relative, _declared) in plan {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("读取压缩包项失败：{e}"))?;
        let target = target_dir.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(long_path(parent))
                .map_err(|e| format!("创建目录 {} 失败：{e}", parent.display()))?;
        }
        let mut output = File::create(long_path(&target))
            .map_err(|e| format!("写入 {} 失败：{e}", target.display()))?;
        loop {
            let read = entry
                .read(&mut buffer)
                .map_err(|e| format!("解压 {} 失败：{e}", decoded))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|e| format!("写入 {} 失败：{e}", target.display()))?;
            report.bytes = report.bytes.saturating_add(read as u64);
        }
        output
            .flush()
            .map_err(|e| format!("刷新 {} 失败：{e}", target.display()))?;
        drop(output);

        report.files += 1;
        on_progress(
            "extracting",
            percent_of(report.bytes, total_bytes, report.files, total_files),
            report.files,
            total_files,
            &decoded,
        );
    }

    Ok(report)
}

/// 导入一个 zip：解压到库目录 → 认主程序 → 写索引 → 广播。
///
/// 这是阻塞操作，命令层要放在阻塞线程里跑。
pub fn import_zip<R: Runtime>(
    app: &AppHandle<R>,
    settings: &Settings,
    zip_path: &str,
    name: Option<String>,
) -> Result<WallpaperEntry, String> {
    let archive_path = PathBuf::from(zip_path);
    if !archive_path.is_file() {
        return Err(format!("压缩包不存在：{zip_path}"));
    }

    let display_name = name
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            archive_path
                .file_stem()
                .map(|stem| stem.to_string_lossy().trim().to_string())
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "未命名壁纸".to_string());

    let root = settings.library_root(app);
    fs::create_dir_all(&root).map_err(|e| format!("创建壁纸库目录失败：{e}"))?;

    let (id, target_dir) = allocate_dir(&root, &display_name, zip_path)?;

    let report = {
        let mut last_emit = Instant::now();
        extract_zip(&archive_path, &target_dir, |phase, percent, files, total, current| {
            if last_emit.elapsed().as_millis() >= PROGRESS_INTERVAL_MS || phase == "reading" {
                last_emit = Instant::now();
                emit_progress(app, phase, percent, files, total, current, &id);
            }
        })?
    };

    emit_progress(app, "finalizing", 100.0, report.files, report.files, "", &id);

    // 认主程序 + 认 Unity
    let info = inspect(&target_dir, &display_name);
    let Some(exe) = info.exe.clone() else {
        // 没找到 exe 说明这个包多半不是壁纸程序：清掉现场，别在列表里留垃圾
        let _ = fs::remove_dir_all(&target_dir);
        return Err("压缩包里没有找到 .exe 主程序，导入已取消".to_string());
    };

    let entry = WallpaperEntry {
        id: id.clone(),
        name: display_name,
        dir: target_dir.to_string_lossy().to_string(),
        exe: exe.to_string_lossy().to_string(),
        exe_name: exe
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default(),
        size_bytes: info.size_bytes,
        file_count: info.file_count,
        imported_at: iso8601(SystemTime::now()),
        unity: info.unity,
        unity_version: info.unity_version,
        broken: false,
        missing_exe: false,
    };

    let mut index = load_index(app);
    index.entries.retain(|item| item.id != entry.id);
    index.entries.push(entry.clone());
    save_index(app, &index)?;

    emit_progress(app, "done", 100.0, info.file_count, info.file_count, "", &id);
    broadcast(app, &list(app, settings));
    Ok(entry)
}

/* ------------------------------------------------------------ 删除 / 改名 */

/// 删除一个壁纸（目录 + 索引项）。
pub fn remove<R: Runtime>(
    app: &AppHandle<R>,
    settings: &Settings,
    id: &str,
) -> Result<Vec<WallpaperEntry>, String> {
    let mut index = load_index(app);
    let entry = index
        .entries
        .iter()
        .find(|item| item.id == id)
        .cloned()
        .ok_or_else(|| format!("找不到壁纸：{id}"))?;

    let dir = PathBuf::from(&entry.dir);
    if dir.is_dir() {
        // 只删库目录里面的东西：索引被手改过也不能让它去删别的地方
        let root = settings.library_root(app);
        if !is_inside(&dir, &root) {
            return Err(format!(
                "{} 不在壁纸库目录里，拒绝删除",
                dir.display()
            ));
        }
        fs::remove_dir_all(long_path(&dir))
            .map_err(|e| format!("删除 {} 失败（壁纸可能正在运行）：{e}", dir.display()))?;
    }

    index.entries.retain(|item| item.id != id);
    save_index(app, &index)?;
    let entries = list(app, settings);
    broadcast(app, &entries);
    Ok(entries)
}

/// 改名（只改展示名，不动目录：目录名是 id，动了会让正在运行的壁纸失联）。
pub fn rename<R: Runtime>(
    app: &AppHandle<R>,
    settings: &Settings,
    id: &str,
    name: &str,
) -> Result<WallpaperEntry, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("名字不能为空".to_string());
    }
    let mut index = load_index(app);
    let entry = index
        .entries
        .iter_mut()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("找不到壁纸：{id}"))?;
    entry.name = trimmed.to_string();
    let updated = entry.clone();
    save_index(app, &index)?;
    broadcast(app, &list(app, settings));
    Ok(updated.revalidated())
}

/* -------------------------------------------------------------- 内部工具 */

/// 目录名用「名字 slug + 短哈希」：同名导入两次也不会互相覆盖。
fn allocate_dir(root: &Path, name: &str, zip_path: &str) -> Result<(String, PathBuf), String> {
    let slug = slugify(name);
    let seed = format!(
        "{}|{}|{}",
        zip_path,
        name,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0)
    );
    let base = format!("{slug}-{:06x}", fnv1a(seed.as_bytes()) & 0x00ff_ffff);

    for attempt in 0..64 {
        let id = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        };
        let dir = root.join(&id);
        if !dir.exists() {
            fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
            return Ok((id, dir));
        }
    }
    Err("同一个名字的壁纸太多了，先删掉几个再导入".to_string())
}

/// 按文件名 / 大小认主程序，并顺手统计目录规模、认 Unity。
fn inspect(dir: &Path, name: &str) -> InspectResult {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(dir, 0, &mut files);

    let mut size_bytes: u64 = 0;
    let mut file_count: u64 = 0;
    for file in &files {
        file_count += 1;
        if let Ok(meta) = fs::metadata(long_path(file)) {
            size_bytes = size_bytes.saturating_add(meta.len());
        }
    }

    let mut exes: Vec<(PathBuf, u64)> = files
        .iter()
        .filter(|path| {
            path.extension()
                .map(|ext| ext.eq_ignore_ascii_case("exe"))
                .unwrap_or(false)
        })
        .filter(|path| {
            // Unity 的崩溃处理器不是主程序
            let stem = path
                .file_stem()
                .map(|value| value.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            !stem.contains("unitycrashhandler")
        })
        .map(|path| {
            let size = fs::metadata(long_path(path)).map(|meta| meta.len()).unwrap_or(0);
            (path.clone(), size)
        })
        .collect();

    // 优先同名 exe（MiSide 目录里的 MiSide.exe），其次最大的那个
    exes.sort_by(|a, b| {
        let a_match = name_matches(a.0.file_stem(), name);
        let b_match = name_matches(b.0.file_stem(), name);
        b_match
            .cmp(&a_match)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });

    let exe = exes.first().map(|(path, _)| path.clone());

    // Unity 判定：同级有 *_Data 目录（里面还有 globalgamemanagers / resources.assets）或 UnityPlayer.dll
    let unity = exe
        .as_ref()
        .map(|path| is_unity_build(path))
        .unwrap_or(false);
    let unity_version = exe
        .as_ref()
        .filter(|_| unity)
        .map(|path| guess_unity_version(path))
        .unwrap_or_default();

    InspectResult {
        exe,
        unity,
        unity_version,
        size_bytes,
        file_count,
    }
}

fn name_matches(stem: Option<&std::ffi::OsStr>, name: &str) -> bool {
    let Some(stem) = stem else { return false };
    let stem = stem.to_string_lossy().to_lowercase();
    let target = name.to_lowercase();
    if stem == target {
        return true;
    }
    let compact_stem: String = stem.chars().filter(|c| c.is_alphanumeric()).collect();
    let compact_target: String = target.chars().filter(|c| c.is_alphanumeric()).collect();
    !compact_target.is_empty() && compact_stem == compact_target
}

fn is_unity_build(exe: &Path) -> bool {
    let Some(parent) = exe.parent() else {
        return false;
    };
    let stem = exe
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let data_dir = parent.join(format!("{stem}_Data"));
    if data_dir.is_dir() {
        return true;
    }
    // 有些工程把 Data 目录改名了，那就看 UnityPlayer.dll
    parent.join("UnityPlayer.dll").is_file()
        || fs::read_dir(parent)
            .map(|entries| {
                entries.flatten().any(|entry| {
                    let path = entry.path();
                    entry
                        .file_name()
                        .to_string_lossy()
                        .to_lowercase()
                        .ends_with("_data")
                        && data_dir_looks_unity(&path)
                })
            })
            .unwrap_or(false)
}

fn data_dir_looks_unity(dir: &Path) -> bool {
    dir.join("globalgamemanagers").is_file()
        || dir.join("resources.assets").is_file()
        || dir.join("data.unity3d").is_file()
}

/// 从 `*_Data/globalgamemanagers` 头部里扫一个 `2021.3.16f1` 这样的版本号。
fn guess_unity_version(exe: &Path) -> String {
    let Some(parent) = exe.parent() else {
        return String::new();
    };
    let stem = exe
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let candidates = [
        parent.join(format!("{stem}_Data")).join("globalgamemanagers"),
        parent.join("UnityPlayer.dll"),
    ];
    for path in candidates {
        if let Some(version) = scan_version(&path) {
            return version;
        }
    }
    String::new()
}

fn scan_version(path: &Path) -> Option<String> {
    let mut file = File::open(long_path(path)).ok()?;
    let mut head = vec![0u8; 512 * 1024];
    let read = file.read(&mut head).ok()?;
    head.truncate(read);

    // 直接对字节做扫描：形如 数字.数字.数字[abfp]数字
    let mut index = 0usize;
    while index < head.len() {
        if !head[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        let mut dots = 0;
        while index < head.len()
            && (head[index].is_ascii_digit() || head[index] == b'.')
        {
            if head[index] == b'.' {
                dots += 1;
            }
            index += 1;
        }
        if dots < 2 {
            continue;
        }
        if index < head.len() && matches!(head[index], b'a' | b'b' | b'f' | b'p') {
            let stage = index;
            index += 1;
            let digits_start = index;
            while index < head.len() && head[index].is_ascii_digit() {
                index += 1;
            }
            if index > digits_start {
                let text = String::from_utf8_lossy(&head[start..index]).to_string();
                // 版本号前面通常紧跟 "m_EditorVersion" 之类的键，抓到的第一段足够可信
                if text.len() <= 24 {
                    let _ = stage;
                    return Some(text);
                }
            }
        }
    }
    None
}

fn collect_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > EXE_SEARCH_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(long_path(dir)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_files(&path, depth + 1, out),
            Ok(kind) if kind.is_file() => out.push(path),
            _ => {}
        }
    }
}

/// 解码 zip 里的文件名：UTF-8 优先，失败回退 GBK。
///
/// 中文 Windows 上用 7-Zip / 资源管理器打的包基本都是 GBK 且**没有**设 UTF-8 标志位，
/// 直接按 UTF-8 读会变成乱码目录名（严重时连 exe 都找不到）。
pub fn decode_entry_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(text) => text.to_string(),
        Err(_) => {
            let (decoded, _, had_errors) = encoding_rs::GBK.decode(raw);
            if had_errors {
                String::from_utf8_lossy(raw).to_string()
            } else {
                decoded.into_owned()
            }
        }
    }
}

/// 把 zip 里的名字变成库目录内的安全相对路径。
///
/// 拒绝：绝对路径（`/` 开头）、盘符、`..`、空字节；`/` 与 `\` 都当分隔符（Windows 上两者都有人用）。
fn safe_relative_path(name: &str) -> Option<PathBuf> {
    // 绝对路径直接拒，而不是"把开头的斜杠去掉当相对路径"—— 后者会让 `/etc/passwd`
    // 变成壁纸目录里的 `etc/passwd`，虽然落不到系统目录，但这类包本身就是坏的。
    if name.starts_with('/') || name.starts_with('\\') {
        return None;
    }
    let trimmed = name.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return None;
    }
    // 盘符形式 `C:...` 直接拒
    if trimmed.len() >= 2
        && trimmed.as_bytes()[1] == b':'
        && trimmed.as_bytes()[0].is_ascii_alphabetic()
    {
        return None;
    }
    let mut out = PathBuf::new();
    for part in trimmed.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return None;
        }
        if part.contains('\0') {
            return None;
        }
        // 目录名里带通配符 / 冒号在 Windows 上非法，直接替换掉而不是失败
        let cleaned: String = part
            .chars()
            .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { c })
            .collect();
        let cleaned = cleaned.trim_end_matches([' ', '.']).to_string();
        if cleaned.is_empty() {
            continue;
        }
        out.push(cleaned);
    }
    if out.as_os_str().is_empty() {
        return None;
    }
    // 再过一遍 Rust 自己的校验：任何父目录 / 前缀组件都算不安全
    if out
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(out)
}

/// 路径超过 Windows 的传统上限时加 `\\?\` 前缀。
fn long_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if path.is_absolute() && text.len() > 240 && !text.starts_with(r"\\?\") {
        PathBuf::from(format!(r"\\?\{}", text))
    } else {
        path.to_path_buf()
    }
}

/// `child` 是否在 `root` 之内（用于删除前的安全检查）。
fn is_inside(child: &Path, root: &Path) -> bool {
    let child = normalize_lexically(child);
    let root = normalize_lexically(root);
    child.starts_with(&root) && child != root
}

/// 只做词法归一（不碰磁盘，也就不需要路径真实存在）。
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in name.chars() {
        if ch.is_alphanumeric() || ch > '\u{7f}' {
            // 中文名保留（Windows 目录名没问题，用户也能一眼认出来）
            slug.push(ch.to_lowercase().next().unwrap_or(ch));
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
        if slug.chars().count() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "wallpaper".to_string()
    } else {
        slug
    }
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn percent_of(done_bytes: u64, total_bytes: u64, done_files: u64, total_files: u64) -> f32 {
    if total_bytes > 0 {
        ((done_bytes as f64 / total_bytes as f64) * 100.0) as f32
    } else if total_files > 0 {
        ((done_files as f64 / total_files as f64) * 100.0) as f32
    } else {
        100.0
    }
}

fn emit_progress<R: Runtime>(
    app: &AppHandle<R>,
    phase: &str,
    percent: f32,
    files: u64,
    total_files: u64,
    current: &str,
    id: &str,
) {
    let _ = app.emit(
        IMPORT_PROGRESS_EVENT,
        ImportProgress {
            phase: phase.to_string(),
            percent: percent.clamp(0.0, 100.0),
            files,
            total_files,
            current: current.to_string(),
            id: id.to_string(),
        },
    );
}

/// `SystemTime` → `2026-01-01T10:00:00Z`。
///
/// 自己算日历是为了不引一个只为格式化时间而存在的依赖（Howard Hinnant 的
/// days-from-civil 反算，含 1970 起的闰年规则）。
pub fn iso8601(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0);

    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gbk_file_names_are_recovered() {
        // "音乐" 的 GBK 编码：D2F4 C0D6
        assert_eq!(decode_entry_name(&[0xd2, 0xf4, 0xc0, 0xd6]), "音乐");
        // 纯 ASCII 与 UTF-8 中文都走原路
        assert_eq!(decode_entry_name(b"MiSide_Data"), "MiSide_Data");
        assert_eq!(decode_entry_name("数据/角色".as_bytes()), "数据/角色");
    }

    #[test]
    fn zip_slip_and_absolute_paths_are_rejected() {
        assert!(safe_relative_path("../evil.exe").is_none());
        assert!(safe_relative_path("a/../../evil.exe").is_none());
        assert!(safe_relative_path("C:/Windows/system32/evil.exe").is_none());
        assert!(safe_relative_path("/etc/passwd").is_none());
        assert!(safe_relative_path("\\windows\\system32\\evil.exe").is_none());
        assert!(safe_relative_path("").is_none());
        assert_eq!(
            safe_relative_path("MiSide_Data/StreamingAssets\\a.bin"),
            Some(PathBuf::from("MiSide_Data").join("StreamingAssets").join("a.bin"))
        );
        // 非盘符位置的冒号与 Windows 非法字符被替换，尾部的点与空格被去掉
        assert_eq!(
            safe_relative_path("abc:def/c. "),
            Some(PathBuf::from("abc_def").join("c"))
        );
    }

    #[test]
    fn long_paths_get_the_verbatim_prefix() {
        let short = Path::new(r"C:\a\b");
        assert_eq!(long_path(short), short);
        let deep = PathBuf::from(format!(r"C:\{}", "x".repeat(300)));
        assert!(long_path(&deep).to_string_lossy().starts_with(r"\\?\"));
    }

    #[test]
    fn inside_check_is_lexical() {
        assert!(is_inside(
            Path::new(r"C:\lib\wp1"),
            Path::new(r"C:\lib")
        ));
        assert!(!is_inside(Path::new(r"C:\lib"), Path::new(r"C:\lib")));
        assert!(!is_inside(
            Path::new(r"C:\lib\..\other"),
            Path::new(r"C:\lib")
        ));
    }

    #[test]
    fn slugify_keeps_chinese_and_strips_noise() {
        assert_eq!(slugify("MiSide 壁纸 v1.0"), "miside-壁纸-v1-0");
        assert_eq!(slugify("   "), "wallpaper");
        assert_eq!(slugify("***"), "wallpaper");
    }

    #[test]
    fn iso8601_matches_known_timestamps() {
        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        // 2026-01-01T00:00:00Z
        assert_eq!(
            iso8601(UNIX_EPOCH + std::time::Duration::from_secs(1_767_225_600)),
            "2026-01-01T00:00:00Z"
        );
        // 2024-02-29（闰日）
        assert_eq!(
            iso8601(UNIX_EPOCH + std::time::Duration::from_secs(1_709_164_800)),
            "2024-02-29T00:00:00Z"
        );
    }

    #[test]
    fn percent_falls_back_to_file_counts() {
        assert_eq!(percent_of(0, 0, 1, 4), 25.0);
        assert_eq!(percent_of(50, 100, 1, 4), 50.0);
        assert_eq!(percent_of(0, 0, 0, 0), 100.0);
    }

    /// 造一个"像 Unity 打包"的壁纸压缩包：主程序 + `*_Data` + 中文目录 + 一条越界路径。
    fn write_wallpaper_zip(path: &Path) {
        let file = File::create(path).expect("创建测试 zip");
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for (name, content) in [
            ("Miside.exe", b"MZ fake pe".as_slice()),
            (
                "Miside_Data/globalgamemanagers",
                b"m_EditorVersion: 2021.3.16f1\n".as_slice(),
            ),
            ("Miside_Data/resources.assets", b"assets".as_slice()),
            ("中文目录/说明.txt", "中文内容".as_bytes()),
            ("../evil.txt", b"escaped".as_slice()),
        ] {
            writer.start_file(name, options).expect("写入条目");
            writer.write_all(content).expect("写内容");
        }
        writer.finish().expect("收尾");
    }

    #[test]
    fn extracts_a_wallpaper_zip_and_recognises_the_unity_program() {
        let root = std::env::temp_dir().join(format!("miside-library-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("建临时目录");
        let zip_path = root.join("Miside 壁纸.zip");
        write_wallpaper_zip(&zip_path);

        let target = root.join("out");
        fs::create_dir_all(&target).expect("建解压目录");
        let mut phases: Vec<String> = Vec::new();
        let report = extract_zip(&zip_path, &target, |phase, _, _, _, _| {
            phases.push(phase.to_string());
        })
        .expect("解压应当成功");

        assert_eq!(report.files, 4, "4 条正常条目，越界那条要被跳过");
        assert_eq!(report.skipped, 1);
        assert!(report.bytes > 0);
        assert!(phases.iter().any(|phase| phase == "reading"));
        assert!(phases.iter().any(|phase| phase == "extracting"));
        assert!(target.join("Miside.exe").is_file());
        assert!(target.join("中文目录").join("说明.txt").is_file());
        assert!(!root.join("evil.txt").exists(), "zip-slip 路径不能落地");

        let info = inspect(&target, "Miside 壁纸");
        assert_eq!(info.file_count, 4);
        assert!(info.size_bytes > 0);
        let exe = info.exe.expect("应当认出主程序");
        assert_eq!(exe.file_name().unwrap().to_string_lossy(), "Miside.exe");
        assert!(info.unity, "有 *_Data 目录就应当认成 Unity");
        assert_eq!(info.unity_version, "2021.3.16f1");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn name_match_beats_a_bigger_helper_program() {
        // Unity 工程里常有个比主程序还大的辅助 exe（崩溃处理器、启动器），
        // 所以「同名优先」必须排在「体积优先」前面
        let root = std::env::temp_dir().join(format!("miside-exe-pick-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("MiSide_Data")).expect("建目录");
        fs::write(root.join("MiSide_Data/globalgamemanagers"), b"2022.3.10f1").expect("写");
        fs::write(root.join("MiSide.exe"), b"tiny").expect("写主程序");
        fs::write(root.join("UnityCrashHandler64.exe"), vec![0u8; 4096]).expect("写");
        fs::write(root.join("helper.exe"), vec![0u8; 8192]).expect("写");

        let info = inspect(&root, "MiSide");
        let exe = info.exe.expect("应当认出主程序");
        assert_eq!(exe.file_name().unwrap().to_string_lossy(), "MiSide.exe");
        assert!(info.unity);
        assert_eq!(info.unity_version, "2022.3.10f1");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_zip_without_any_exe_is_reported_as_such() {
        let root = std::env::temp_dir().join(format!("miside-noexe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("建目录");
        fs::write(root.join("readme.txt"), b"no program here").expect("写");

        let info = inspect(&root, "随便");
        assert!(info.exe.is_none());
        assert!(!info.unity);

        let _ = fs::remove_dir_all(&root);
    }
}
