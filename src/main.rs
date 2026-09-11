/*
 * Copyright (c) 2026, LJason. All Rights Reserved.
 */

//! `video-mapper` 命令行主程序入口。
//!
//! 负责协调 CLI 参数解析、日志系统初始化、异步运行时驱动，
//! 并编排配置加载、目录扫描、ffprobe 探测缓存、旧数据修剪及 M3U 播放列表生成流程。

mod config;
mod db;
mod ffprobe;
mod m3u;
mod scanner;

use std::{
    borrow::Cow,
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use clap::Parser;
use futures::{StreamExt, TryStreamExt};
use tracing_subscriber::{EnvFilter, fmt::time};

use crate::{
    config::Config,
    db::{Database, VideoRecord},
};

/// 视频映射工具的命令行参数定义。
#[derive(Debug, Parser)]
#[command(name = "vmap", version, about = "扫描视频文件并生成 m3u 播放列表", long_about = None)]
struct Cli {
    /// 配置文件路径
    #[arg(short, long, default_value = "~/.config/playlist.json")]
    config: PathBuf,

    /// SQLite 数据库文件路径
    #[arg(short, long, default_value = "~/.cache/video-mapper/cache.db")]
    db: PathBuf,

    /// 并发 ffprobe 调用数，0 表示自动检测
    #[arg(short, long, default_value_t = 0)]
    threads: usize,

    /// 显示详细日志
    #[arg(short, long)]
    verbose: bool,
}

/// 程序主入口，拉起多线程 Tokio 异步运行时。
///
/// # Errors
/// - 日志初始化失败；
/// - 配置文件读取或解析失败；
/// - 数据库初始化失败；
/// - 目录扫描任务 panic（join 错误）；
/// - 任一视频的数据库写入失败；
/// - 过期记录清理、方向映射查询或 m3u 生成失败。
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // 初始化日志系统
    let filter = if cli.verbose { EnvFilter::new("debug,sqlx=error") } else { EnvFilter::new("info,sqlx=error") };
    tracing_subscriber::fmt().with_env_filter(filter).with_timer(time::OffsetTime::local_rfc_3339().context("无法获得本地时差！")?).init();
    // 加载配置文件
    let configs = Arc::new(Config::from_file(&cli.config)?);
    tracing::debug!("已加载配置文件：{}", cli.config.display());
    // 初始化数据库
    let db = Database::init_pool(&cli.db).await?;
    tracing::debug!("已初始化数据库：{}", cli.db.display());

    // 收集并去重所有根路径
    let all_paths: Vec<PathBuf> = configs.iter().flat_map(|cfg| cfg.list.iter()).flat_map(|item| item.paths.iter().cloned()).collect();
    let roots = Arc::new(scanner::deduplicate_roots(all_paths));
    tracing::debug!("已收集 {} 个根路径：{:?}", roots.len(), roots);

    // 扫描所有根路径，收集视频文件绝对路径集合
    let roots_for_scan = Arc::clone(&roots);
    let video_paths = tokio::task::spawn_blocking(move || scanner::scan_videos(&roots_for_scan)).await.context("目录扫描任务失败")?;
    tracing::debug!("已扫描 {} 个视频文件", video_paths.len());
    let video_paths = Arc::new(video_paths);

    // 预取数据库中的已有路径
    let cached_paths = Arc::new(db.get_all_paths().await?);
    tracing::debug!("已预取 {} 个数据库中已有的视频路径", cached_paths.len());

    // 并发探测未缓存的视频
    let concurrency = if cli.threads == 0 { std::thread::available_parallelism().map_or(1, std::num::NonZero::get) } else { cli.threads };
    let db_ref = &db;
    futures::stream::iter(video_paths.iter().filter(|p| !cached_paths.contains(*p)))
        .map(|path| process_video(db_ref, path))
        .buffer_unordered(concurrency)
        .try_collect::<Vec<()>>()
        .await?;

    // 清理数据库中已不存在的视频条目（需先检查所有根路径可访问性）
    if all_roots_accessible(&roots) {
        let deleted = db.delete_missing(&video_paths).await?;
        tracing::info!("已清理 {deleted} 条过期记录");
    } else {
        tracing::warn!("部分根路径不可访问，跳过数据库清理，防止误删");
    }

    // 加载方向映射（需要读到本轮新写入的记录，故此处重新查询）
    let orientation_map = Arc::new(db.get_all_orientations().await?);

    // 生成 m3u 播放列表（阻塞操作，放入 spawn_blocking）
    for config in configs.iter() {
        let config_for_task = config.clone();
        let vp = Arc::clone(&video_paths);
        let om = Arc::clone(&orientation_map);
        tokio::task::spawn_blocking(move || m3u::generate_playlists(&config_for_task, &vp, &om)).await.context("m3u 生成任务失败")??;
    }

    Ok(())
}

/// 处理单个视频文件：调用 ffprobe 并将结果写入数据库。
///
/// # Errors
/// - `spawn_blocking` 任务 panic（join 错误）；
/// - 数据库写入失败。
///
/// ffprobe 调用失败仅记录 `warn` 并返回 `Ok(())`——单个视频无法解析不应中断整体扫描。
async fn process_video(db: &Database, video_path: &Path) -> Result<()> {
    // `spawn_blocking` 要求 `'static` 输入，此处必须 owned
    let owned_for_probe = video_path.to_path_buf();
    let probe_result = tokio::task::spawn_blocking(move || ffprobe::probe(&owned_for_probe)).await.context("ffprobe 任务执行失败")?;

    let info = match probe_result {
        Ok(info) => info,
        Err(e) => {
            tracing::warn!("无法解析视频 {}：{e:#}", video_path.display());
            return Ok(());
        }
    };
    tracing::debug!("已解析视频 {}：{:?}", video_path.display(), info.orientation);

    let record = VideoRecord { path: video_path.to_path_buf(), width: info.width, height: info.height, orientation: info.orientation };

    db.upsert_video(&record).await?;
    Ok(())
}

/// 检查所有根路径是否可访问（目录存在且可读）。
#[must_use = "返回值是判定结果，忽略它意味着执行了无意义的文件系统探测"]
fn all_roots_accessible<P: AsRef<Path>>(roots: &[P]) -> bool {
    roots.iter().all(|root| std::fs::read_dir(root.as_ref()).is_ok())
}

/// 将路径开头的 `~` 展开为当前用户的家目录。
///
/// # Errors
/// 当路径以 `~` 开头但无法获取 `HOME` 环境变量，或路径包含非 UTF-8 字符时返回错误。
fn expand_tilde(path: &Path) -> Result<Cow<'_, Path>> {
    let Ok(rest) = path.strip_prefix("~") else {
        return Ok(Cow::Borrowed(path));
    };

    let home_dir = env::home_dir().context("获取用户主目录失败")?;

    // 当 rest 为空路径时（即入参仅为 "~"），直接返回 home 目录
    if rest.as_os_str().is_empty() { Ok(Cow::Owned(home_dir)) } else { Ok(Cow::Owned(home_dir.join(rest))) }
}
