/*
 * Copyright (c) 2026, LJason. All Rights Reserved.
 */

//! 目录扫描与路径去重模块。

use std::{
    collections::HashSet,
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use walkdir::WalkDir;

/// 支持的视频文件扩展名（小写，不含点）。
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "avi", "mov", "flv", "ts", "m2ts", "rmvb", "webm", "wmv", "m4v", "mpg", "mpeg"];

/// 扫描时跳过其子树的目录名。
///
/// 目前仅包含 `lost+found`——ext2/3/4 文件系统在挂载点根目录自动创建的元数据目录，权限通常为 `0700` 且属主为 root。
/// 非 root 进程遍历时会触发 `EACCES`，产生无意义的警告。
/// 该目录永远不包含用户视频文件，跳过安全。
///
/// 匹配基于**目录条目名**（不含路径），因此任意深度的同名目录都会被跳过。
const SKIPPED_DIR_NAMES: &[&str] = &["lost+found"];

/// 收集并去重所有需要扫描的根路径。
///
/// 处理流程：
/// 1. 将每个路径规范化为绝对路径（不检查存在性，仅规范化 `.`、`..` 与尾部斜杠）。
/// 2. 去除被其他根路径包含的子路径：若路径 A 是路径 B 的祖先，则移除 B。
/// 3. 去除重复路径。
///
/// # Errors
/// 本函数不返回错误：无法规范化的路径会被记录为 `warn` 级别日志并跳过。
#[must_use]
pub fn deduplicate_roots(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut normalized: Vec<PathBuf> = paths
        .into_iter()
        .filter_map(|p| match normalize_root(&p) {
            Ok(n) => Some(n),
            Err(e) => {
                tracing::warn!("跳过无效根路径 {}：{e:#}", p.display());
                None
            }
        })
        .collect();

    // 按 (组件数, 路径) 排序：父路径先于子路径；相等路径相邻
    // `sort_unstable_by` 在严格全序下可以安全使用，且省去稳定排序的临时缓冲
    normalized.sort_unstable_by(|a, b| a.components().count().cmp(&b.components().count()).then_with(|| a.cmp(b)));

    // 去除相邻重复项。相等路径有相同的组件数与路径值，排序后必然相邻
    normalized.dedup();

    // 单遍过滤：仅保留不是任何已保留根的子路径的项
    let mut roots: Vec<PathBuf> = Vec::with_capacity(normalized.len());
    for path in normalized {
        if !roots.iter().any(|root| path.starts_with(root)) {
            roots.push(path);
        }
    }

    roots
}

/// 将路径规范化为绝对路径并去除尾部斜杠（根路径 `/` 除外）。
///
/// # Errors
/// - 路径为空；
/// - 当前工作目录无法确定，导致 [`std::path::absolute`] 失败。
fn normalize_root(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("路径为空");
    }

    // 转换为绝对路径，不要求路径存在
    let abs = std::path::absolute(path).with_context(|| format!("无法获取绝对路径：{}", path.display()))?;

    // `components()` 会忽略尾部斜杠（根路径例外，它是单个 `RootDir`）
    // 因此 `collect::<PathBuf>()` 得到规范形式。这是每次启动的一次性开销
    Ok(abs.components().collect())
}

/// 递归扫描给定根路径列表，收集所有视频文件的绝对路径。
///
/// 该函数为**阻塞**操作，应在 `spawn_blocking` 中调用，避免阻塞异步运行时。
///
/// # 行为
///
/// - 对每个根路径使用 [`WalkDir`] 递归遍历，不跟随符号链接。
/// - 仅收集扩展名在 [`VIDEO_EXTENSIONS`] 中（不区分大小写）的文件。
/// - 遍历过程中发生的 I/O 错误（如权限不足、目录不存在）会被记录为警告并跳过，不会导致整个扫描失败。
/// - 返回的 `HashSet` 自动去除因路径重叠导致的重复文件。
///
/// # Panics
/// 该函数不会 panic。
#[must_use]
pub fn scan_videos(roots: &[PathBuf]) -> HashSet<PathBuf> {
    let mut videos = HashSet::new();

    for root in roots {
        // 每个根路径单独遍历，避免一个根路径错误影响其他根路径
        let walker = WalkDir::new(root).follow_links(false).into_iter().filter_entry(|entry| !is_skipped_dir(entry)).filter_map(|e| match e {
            Ok(entry) => Some(entry),
            Err(err) => {
                tracing::warn!("遍历目录 {} 时出错：{err}", root.display());
                None
            }
        });

        for entry in walker {
            // 仅处理普通文件；符号链接（`follow_links(false)` 下 `file_type()` 报告为 symlink）会被此检查排除。
            if !entry.file_type().is_file() {
                continue;
            }

            if is_video_file(entry.path()) {
                // `into_path()` 移动 `DirEntry` 内部拥有的 `PathBuf`，避免 `to_path_buf()` 的额外分配
                videos.insert(entry.into_path());
            }
        }
    }

    videos
}

/// 判断给定目录条目是否属于需要跳过子树的目录。
///
/// 仅对目录类型返回 `true`：同名的普通文件不会被跳过。
/// `entry.file_name()` 返回条目自身的名字（不含路径），与 [`SKIPPED_DIR_NAMES`] 逐项比较。
#[inline]
fn is_skipped_dir(entry: &walkdir::DirEntry) -> bool {
    entry.file_type().is_dir() && SKIPPED_DIR_NAMES.iter().any(|name| entry.file_name() == OsStr::new(name))
}

/// 判断给定路径是否具有视频文件扩展名（ASCII 大小写不敏感）。
///
/// 非 UTF-8 的扩展名（Unix 下可能）会返回 `false`，因为 [`OsStr::to_str`] 对非 UTF-8 返回 `None`。
#[must_use]
fn is_video_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(OsStr::to_str) else {
        return false;
    };
    VIDEO_EXTENSIONS.iter().any(|known| ext.eq_ignore_ascii_case(known))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    use std::fs;
    use std::fs::File;

    /// 创建临时目录结构用于测试。
    fn create_test_tree() -> PathBuf {
        let base = std::env::temp_dir().join(format!("vmap_test_{}", std::process::id()));
        if base.exists() {
            fs::remove_dir_all(&base).expect("清理旧测试目录失败");
        }
        fs::create_dir_all(&base).expect("创建测试目录失败");
        base
    }

    #[test]
    fn test_deduplicate_roots_simple() {
        let paths = vec![PathBuf::from("/AAA/BBB"), PathBuf::from("/AAA"), PathBuf::from("/CCC")];
        let roots = deduplicate_roots(paths);
        assert_eq!(roots, vec![PathBuf::from("/AAA"), PathBuf::from("/CCC")]);
    }

    #[test]
    fn test_deduplicate_roots_redundant_slashes() {
        let paths = vec![PathBuf::from("/AAA/"), PathBuf::from("/AAA//BBB/")];
        let roots = deduplicate_roots(paths);
        assert_eq!(roots, vec![PathBuf::from("/AAA")]);
    }

    #[test]
    fn test_scan_videos_extension_filter_and_dedup() {
        let base = create_test_tree();
        let sub_dir = base.join("sub");
        fs::create_dir_all(&sub_dir).expect("创建子目录失败");

        // 创建视频文件（不同扩展名，大小写混合）
        File::create(base.join("movie.MP4")).expect("创建文件失败");
        File::create(base.join("clip.mkv")).expect("创建文件失败");
        File::create(sub_dir.join("video.webm")).expect("创建文件失败");

        // 创建非视频文件
        File::create(base.join("notes.txt")).expect("创建文件失败");
        File::create(sub_dir.join("image.jpg")).expect("创建文件失败");

        let roots = vec![base.clone()];
        let videos = scan_videos(&roots);

        assert_eq!(videos.len(), 3);
        assert!(videos.contains(&std::path::absolute(base.join("movie.MP4")).unwrap()));
        assert!(videos.contains(&std::path::absolute(base.join("clip.mkv")).unwrap()));
        assert!(videos.contains(&std::path::absolute(sub_dir.join("video.webm")).unwrap()));

        // 清理
        fs::remove_dir_all(&base).expect("清理测试目录失败");
    }

    #[test]
    fn test_scan_videos_handles_missing_root() {
        let missing = PathBuf::from("/nonexistent/path/for/test");
        let roots = vec![missing];
        let videos = scan_videos(&roots);
        assert!(videos.is_empty());
    }

    #[test]
    fn test_deduplicate_roots_relative_paths() {
        // 测试相对路径规范化。
        let paths = vec![PathBuf::from("./AAA"), PathBuf::from("./AAA/BBB")];
        let roots = deduplicate_roots(paths);
        // 第一个路径被规范化为 /home/user/AAA，应包含第二个路径。
        assert_eq!(roots.len(), 1);
        assert!(roots[0].ends_with("AAA"));
    }
}
