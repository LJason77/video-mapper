/*
 * Copyright (c) 2026, LJason. All Rights Reserved.
 */

//! m3u 播放列表生成模块，负责将本地视频路径转换为远程 URL 并写入 m3u 文件。

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rand::seq::SliceRandom;

use crate::{
    config::{Config, ListItem},
    db::Orientation,
};

/// 百分号编码时需要被编码的 ASCII 字节集合。
///
/// 基于 [`NON_ALPHANUMERIC`] 移除 URL 路径中安全可用的字符（`-`、`_`、`.`、`~`、`/`）后得到。
/// "**需要**编码的字节"，使用时注意 `contains` 返回 `true` 表示需要编码。
const NEEDS_ENCODING: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~').remove(b'/');

/// 判断单个字节是否**无需**百分号编码。
///
/// 与 [`NEEDS_ENCODING`] 表达的集合互补：
/// - 返回 `true`：字节位于 URL 路径安全字符集（`A-Z a-z 0-9 - _ . ~ /`）内；
/// - 返回 `false`：字节需要编码。
///
/// 对 `>= 0x80` 的字节，[`u8::is_ascii_alphanumeric`] 与 `matches!` 均不匹配，因此返回 `false`。
/// 与 [`utf8_percent_encode`] 对所有非 ASCII 字节进行 UTF-8 编码后再百分号编码的行为一致。
#[inline]
const fn is_url_safe_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/')
}

/// 多个 playlist 生成过程中**不变**的共享上下文。
struct PlaylistContext<'a> {
    /// 所有已扫描到的视频绝对路径集合。
    video_paths: &'a HashSet<PathBuf>,
    /// 视频路径到方向的映射，用于方向过滤。
    orientation_map: &'a HashMap<PathBuf, Orientation>,
    /// 本地路径前缀，用于从视频绝对路径推导相对路径。
    replace_path: &'a Path,
    /// `WebDAV` 基础 URL，已去除尾部斜杠。
    base_url: &'a str,
}

impl<'ctx> PlaylistContext<'ctx> {
    /// 收集属于 `item` 且通过 `accept` 过滤的视频的远程 URL。
    ///
    /// # 返回值
    /// 返回一个二元组：
    /// - 收集到的 URL 列表（已按 [`playlist`] 顺序排列，未打乱）；
    /// - 构建 m3u 内容所需的精确字节容量，含 `#EXTM3U\n` 与每条记录的固定开销。
    fn collect_urls<F: Fn(&Path) -> bool>(&self, item: &ListItem, accept: F) -> (Vec<Cow<'ctx, str>>, usize) {
        let mut urls: Vec<Cow<'ctx, str>> = Vec::with_capacity(self.video_paths.len());
        let mut capacity = "#EXTM3U\n".len();

        for video_path in self.video_paths {
            if !item.paths.iter().any(|root| video_path.starts_with(root)) {
                continue;
            }
            if !accept(video_path) {
                continue;
            }

            match build_remote_url(self.base_url, self.replace_path, video_path) {
                Ok(url) => {
                    // 每条记录 = "#EXTINF:-1,\n" + url + "\n"
                    capacity += "#EXTINF:-1,\n".len() + url.len() + 1;
                    urls.push(url);
                }
                Err(e) => tracing::warn!("跳过无法构建远程 URL 的视频 {}：{e:#}", video_path.display()),
            }
        }

        (urls, capacity)
    }
}

/// 为给定配置项生成所有 m3u 播放列表。
///
/// # Errors
/// 当任一 [`ListItem`] 的 m3u 文件生成失败时返回错误，附带失败的 m3u 路径上下文。
pub fn generate_playlists(config: &Config, video_paths: &HashSet<PathBuf>, orientation_map: &HashMap<PathBuf, Orientation>) -> Result<()> {
    // 循环外创建 RNG，避免每个 playlist 都访问一次 TLS
    let mut rng = rand::rng();

    // 循环外预计算 replace 前缀与 base URL，避免每个 URL 重算
    let replace_path = Path::new(&config.replace);
    let base_url = config.head.trim_end_matches('/');
    let ctx = PlaylistContext { video_paths, orientation_map, replace_path, base_url };

    for item in &config.list {
        generate_single_playlist(item, &ctx, &mut rng).with_context(|| format!("生成 m3u 文件失败：{}", item.m3u.display()))?;
    }
    Ok(())
}

/// 为单个 [`ListItem`] 生成 m3u 文件。
///
/// # 方向过滤
///
/// 根据 `item.orientation` 分两个分支，各自调用 [`PlaylistContext::collect_urls`]：
/// - `Some(filter)` 分支传入方向匹配闭包；
/// - `None` 分支传入恒真闭包。
///
/// 过滤逻辑在调用点确定，`collect_urls` 的循环体内**没有** `Option` 判别。
///
/// # Errors
/// 当路径转换、目录创建或文件写入失败时返回错误。
fn generate_single_playlist(item: &ListItem, ctx: &PlaylistContext<'_>, rng: &mut impl rand::Rng) -> Result<()> {
    // 两个分支在编译期确定过滤路径，循环体内无 Option 判别
    let (mut urls, content_capacity) = match item.orientation {
        Some(filter) => ctx.collect_urls(item, |p| ctx.orientation_map.get(p) == Some(&filter)),
        None => ctx.collect_urls(item, |_| true),
    };

    // 打乱：使用传入的 RNG，避免每次 TLS 访问
    urls.shuffle(rng);

    // 单次分配构建完整内容：容量已在 collect_urls 中精确累加
    let mut content = String::with_capacity(content_capacity);
    content.push_str("#EXTM3U\n");
    for url in &urls {
        content.push_str("#EXTINF:-1,\n");
        content.push_str(url);
        content.push('\n');
    }

    // 确保父目录存在
    if let Some(parent) = item.m3u.parent() {
        fs::create_dir_all(parent).with_context(|| format!("创建 m3u 文件父目录失败：{}", parent.display()))?;
    }

    // 写入文件：`File::write_all` 直达内核，无需显式 flush
    let mut m3u = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&item.m3u)
        .with_context(|| format!("写入 m3u 文件失败：{}", item.m3u.display()))?;
    m3u.write_all(content.as_bytes()).with_context(|| format!("写入 m3u 文件失败：{}", item.m3u.display()))?;

    Ok(())
}

/// 根据上下文与视频绝对路径构建远程访问 URL。
///
/// # Errors
/// - 路径无法去除 `replace_path` 前缀（视频不在该前缀下）；
/// - 相对路径不是有效 UTF-8。
fn build_remote_url<'a>(base_url: &str, replace_path: &Path, video_path: &'a Path) -> Result<Cow<'a, str>> {
    let relative_path = video_path
        .strip_prefix(replace_path)
        .with_context(|| format!("视频路径 {} 不在 replace 前缀 {} 下", video_path.display(), replace_path.display()))?;

    let relative_str = relative_path.to_str().ok_or_else(|| anyhow::anyhow!("相对路径不是有效的 UTF-8：{}", relative_path.display()))?;

    // 快路径：字节级扫描判断是否含有需要编码的字节
    // `is_url_safe_byte` 是 `#[inline] const fn`，会被 LLVM 展开为位运算，且 `bytes().any()` 在长字符串上可能被自动向量化
    let needs_encoding = relative_str.bytes().any(|b| !is_url_safe_byte(b));

    let encoded: Cow<'a, str> =
        if needs_encoding { Cow::Owned(utf8_percent_encode(relative_str, NEEDS_ENCODING).to_string()) } else { Cow::Borrowed(relative_str) };

    // `format!` 每次分配一次 String，无法避免（最终结果要拼接 base + '/' + encoded），但 encoded 本身的字符串内容不再重复拷贝
    Ok(Cow::Owned(format!("{base_url}/{encoded}")))
}
