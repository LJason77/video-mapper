/*
 * Copyright (c) 2026, LJason. All Rights Reserved.
 */

//! SQLite 数据库访问层，负责连接池管理、元数据缓存与过期条目清理。

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Result};
use serde::Deserialize;
use sqlx::{
    QueryBuilder, Sqlite, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use crate::expand_tilde;

/// 视频方向枚举。
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, sqlx::Type)]
#[sqlx(rename_all = "PascalCase")]
pub enum Orientation {
    /// 横屏（宽度 ≥ 高度）。
    Horizontal,
    /// 竖屏（宽度 < 高度）。
    Vertical,
}

impl Orientation {
    /// 返回枚举对应的字符串表示，用于数据库存储。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Horizontal => "Horizontal",
            Self::Vertical => "Vertical",
        }
    }
}

impl FromStr for Orientation {
    type Err = anyhow::Error;

    /// 从数据库文本表示解析方向。
    ///
    /// # Errors
    /// 当字符串不是 `"Horizontal"` 或 `"Vertical"` 时返回错误
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Horizontal" => Ok(Self::Horizontal),
            "Vertical" => Ok(Self::Vertical),
            _ => anyhow::bail!("方向字符串无效：{s}"),
        }
    }
}

/// 视频元数据记录，对应数据库 `videos` 表中的一行。
#[derive(Debug, Clone, PartialEq)]
pub struct VideoRecord {
    /// 视频文件的绝对路径（UTF-8 编码）。
    pub path: PathBuf,
    /// 视频宽度（像素）。
    pub width: i64,
    /// 视频高度（像素）。
    pub height: i64,
    /// 视频方向。
    pub orientation: Orientation,
}

/// 数据库连接池封装
#[derive(Debug, Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// 初始化数据库连接池并创建表结构。
    ///
    /// # Errors
    /// 当数据库文件无法创建或打开、连接池建立失败、建表语句执行失败时返回错误。
    pub async fn init_pool(db_path: &Path) -> Result<Self> {
        let db_path = expand_tilde(db_path)?;

        // 确保数据库文件父目录存在
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("创建数据库父目录失败：{}", parent.display()))?;
        }

        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            // WAL (Write-Ahead Logging) 模式：避免读写互相阻塞，极大降低 Lock Contention
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            // Synchronous NORMAL 级别：仅在关键的 Checkpoint 触发真实的 fsync 刷盘，大幅减少内核上下文切换
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(10));

        let pool = SqlitePoolOptions::new().max_connections(5).connect_with(options).await.context("创建 SQLite 连接池失败")?;

        // 基于 WITHOUT ROWID 优化的物理存储布局
        let init_sql = "
            CREATE TABLE IF NOT EXISTS videos
            (
                path TEXT PRIMARY KEY,
                width INTEGER NOT NULL,
                height INTEGER NOT NULL,
                orientation TEXT NOT NULL CHECK (orientation IN ('Horizontal', 'Vertical'))
            ) WITHOUT ROWID, STRICT;";

        // 将 SQL 解析树和执行计划缓存在 SQLite 引擎内
        sqlx::query(init_sql).execute(&pool).await.context("创建 videos 表失败")?;

        Ok(Self { pool })
    }

    /// 根据路径查询视频元数据。
    ///
    /// # Errors
    /// 当路径不是有效 UTF-8 或 SQL 查询失败时返回错误。
    #[allow(unused)]
    pub async fn get_video(&self, path: &Path) -> Result<Option<VideoRecord>> {
        let path_str = path.to_str().ok_or_else(|| anyhow::anyhow!("路径不是有效的 UTF-8：{}", path.display()))?;

        let row = sqlx::query_as::<_, (String, i64, i64, Orientation)>("SELECT path, width, height, orientation FROM videos WHERE path = ?1")
            .bind(path_str)
            .fetch_optional(&self.pool)
            .await
            .context("按路径查询视频失败")?;

        Ok(row.map(|(path, width, height, orientation)| VideoRecord { path: PathBuf::from(path), width, height, orientation }))
    }

    /// 一次性拉取数据库中所有视频路径，供上游在内存中判断缓存命中。
    ///
    /// # Errors
    /// 当 SQL 查询失败时返回错误。
    pub async fn get_all_paths(&self) -> Result<HashSet<PathBuf>> {
        let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM videos").fetch_all(&self.pool).await.context("查询所有视频路径失败")?;
        Ok(paths.into_iter().map(PathBuf::from).collect())
    }

    /// 插入或更新一条视频元数据记录。
    ///
    /// # Errors
    /// 当路径不是有效 UTF-8 或 SQL 执行失败时返回错误。
    pub async fn upsert_video(&self, record: &VideoRecord) -> Result<()> {
        let path_str = record.path.to_str().ok_or_else(|| anyhow::anyhow!("路径不是有效的 UTF-8：{}", record.path.display()))?;

        sqlx::query(
            "INSERT INTO videos (path, width, height, orientation)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(path) DO UPDATE SET
                width = excluded.width,
                height = excluded.height,
                orientation = excluded.orientation",
        )
        .bind(path_str)
        .bind(record.width)
        .bind(record.height)
        .bind(record.orientation.as_str())
        .execute(&self.pool)
        .await
        .context("插入或更新视频记录失败")?;

        Ok(())
    }

    /// 删除数据库中所有不在 `current_paths` 中的记录，并返回删除数量。
    ///
    /// # Errors
    /// 当事务开始、查询、删除或提交过程中发生 SQL 错误，或路径不是有效 UTF-8 时返回错误。
    pub async fn delete_missing(&self, current_paths: &HashSet<PathBuf>) -> Result<usize> {
        /// 单次 `DELETE ... IN (...)` 中绑定的最大参数数量
        const DELETE_CHUNK_SIZE: usize = 500;

        // 一次查询拉取全部数据库路径
        let db_paths: Vec<String> = sqlx::query_scalar("SELECT path FROM videos").fetch_all(&self.pool).await.context("查询视频记录失败")?;

        // 求差集：DB 中有而当前集合中无的路径即为待删除项
        let to_delete: Vec<&str> = db_paths.iter().filter_map(|p| (!current_paths.contains(Path::new(p.as_str()))).then_some(p.as_str())).collect();
        if to_delete.is_empty() {
            return Ok(0);
        }

        // 分块批量删除
        let mut tx = self.pool.begin().await.context("开始事务失败")?;
        let mut deleted = 0u64;

        for chunk in to_delete.chunks(DELETE_CHUNK_SIZE) {
            let mut builder: QueryBuilder<Sqlite> = QueryBuilder::new("DELETE FROM videos WHERE path IN (");
            let mut separated = builder.separated(", ");
            for path in chunk {
                separated.push_bind(*path);
            }
            separated.push_unseparated(")");

            let result = builder.build().execute(&mut *tx).await.context("批量删除缺失的视频记录失败")?;
            deleted += result.rows_affected();
        }

        tx.commit().await.context("提交事务失败")?;
        Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
    }

    /// 获取所有视频路径及其方向的映射。
    ///
    /// 该方法一次性加载全部记录，用于后续按方向过滤生成 m3u 列表，避免在生成过程中逐条查询数据库，减少 SQL 往返次数。
    ///
    /// # Errors
    /// 当 SQL 查询失败或数据库中存在无效方向字符串时返回错误。
    pub async fn get_all_orientations(&self) -> Result<HashMap<PathBuf, Orientation>> {
        let rows = sqlx::query_as::<_, (String, Orientation)>("SELECT path, orientation FROM videos")
            .fetch_all(&self.pool)
            .await
            .context("查询所有视频方向失败")?;

        Ok(rows.into_iter().map(|(path, orientation)| (PathBuf::from(path), orientation)).collect())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    async fn setup_pool() -> Database {
        // 测试使用单连接内存数据库，避免多连接导致的隔离问题。
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.expect("创建测试池失败");

        // 基于 WITHOUT ROWID 优化的物理存储布局
        let init_sql = "
            CREATE TABLE IF NOT EXISTS videos
            (
                path TEXT PRIMARY KEY,
                width INTEGER NOT NULL,
                height INTEGER NOT NULL,
                orientation TEXT NOT NULL CHECK (orientation IN ('Horizontal', 'Vertical'))
            ) WITHOUT ROWID, STRICT;";

        // 将 SQL 解析树和执行计划缓存在 SQLite 引擎内
        sqlx::query(init_sql).execute(&pool).await.expect("创建 videos 表失败");

        Database { pool }
    }

    #[tokio::test]
    async fn test_upsert_and_get() {
        let db = setup_pool().await;
        let record = VideoRecord { path: PathBuf::from("/test/video.mp4"), width: 1920, height: 1080, orientation: Orientation::Horizontal };
        db.upsert_video(&record).await.unwrap();
        let fetched = db.get_video(Path::new("/test/video.mp4")).await.unwrap().unwrap();
        assert_eq!(fetched, record);
    }

    #[tokio::test]
    async fn test_delete_missing() {
        let db = setup_pool().await;
        let record1 = VideoRecord { path: PathBuf::from("/test/keep.mp4"), width: 1280, height: 720, orientation: Orientation::Horizontal };
        let record2 = VideoRecord { path: PathBuf::from("/test/remove.mp4"), width: 720, height: 1280, orientation: Orientation::Vertical };
        db.upsert_video(&record1).await.unwrap();
        db.upsert_video(&record2).await.unwrap();

        let mut current = HashSet::new();
        current.insert(PathBuf::from("/test/keep.mp4"));
        let deleted = db.delete_missing(&current).await.unwrap();
        assert_eq!(deleted, 1);
        assert!(db.get_video(Path::new("/test/keep.mp4")).await.unwrap().is_some());
        assert!(db.get_video(Path::new("/test/remove.mp4")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_get_all_orientations() {
        let db = setup_pool().await;
        let record1 = VideoRecord { path: PathBuf::from("/test/h.mp4"), width: 1920, height: 1080, orientation: Orientation::Horizontal };
        let record2 = VideoRecord { path: PathBuf::from("/test/v.mp4"), width: 1080, height: 1920, orientation: Orientation::Vertical };
        db.upsert_video(&record1).await.unwrap();
        db.upsert_video(&record2).await.unwrap();

        let map = db.get_all_orientations().await.unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map[Path::new("/test/h.mp4")], Orientation::Horizontal);
        assert_eq!(map[Path::new("/test/v.mp4")], Orientation::Vertical);
    }
}
