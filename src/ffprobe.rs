/*
 * Copyright (c) 2026, LJason. All Rights Reserved.
 */

//! ffprobe 调用与输出解析模块，负责获取视频流的宽高并判断方向。

use std::{path::Path, process::Command};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::db::Orientation;

/// 从 ffprobe 提取的视频元信息。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoInfo {
    /// 视频宽度（像素）。
    pub width: i64,
    /// 视频高度（像素）。
    pub height: i64,
    /// 视频方向。
    pub orientation: Orientation,
}

/// ffprobe JSON 输出的最小解析视图。
///
/// 只声明实际使用的字段，未声明的字段在反序列化时被**跳过**，
/// 避免 `serde_json::Value` 的树形中间表示带来的分配。
#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    /// 视频流列表。缺失时视为空列表，由后续校验统一报错。
    #[serde(default)]
    streams: Vec<StreamEntry>,
}

/// ffprobe 单个流的字段子集。
#[derive(Debug, Deserialize)]
struct StreamEntry {
    /// 视频宽度（像素）。`null` 或缺失时为 `None`。
    width: Option<i64>,
    /// 视频高度（像素）。`null` 或缺失时为 `None`。
    height: Option<i64>,
}

/// 调用 ffprobe 获取视频宽高并判断方向。
///
/// 该函数为**阻塞**操作，必须在 `spawn_blocking` 中调用，避免阻塞异步运行时。
///
/// # Errors
/// - `ffprobe` 无法启动（未安装、不在 `PATH`、`EMFILE` 耗尽）；
/// - `ffprobe` 退出状态非零（文件不存在、格式不支持、权限不足）；
/// - JSON 输出无法解析；
/// - JSON 中无视频流，或 `width` / `height` 缺失、非正整数。
pub fn probe(video_path: &Path) -> Result<VideoInfo> {
    let output = Command::new("ffprobe")
        .args(["-hide_banner", "-v", "error", "-print_format", "json", "-show_entries", "stream=width,height", "-select_streams", "v:0"])
        .arg(video_path)
        .output()
        .context("启动 ffprobe 进程失败")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("ffprobe 执行失败（退出状态 {}）：{}", output.status, stderr.trim()));
    }

    parse_ffprobe_json(&output.stdout)
}

/// 解析 ffprobe 的 JSON 输出，提取首个视频流的宽高并判断方向。
///
/// # Errors
/// - JSON 语法错误；
/// - `streams` 数组为空；
/// - `width` 或 `height` 缺失、为 `null`、或值 `<= 0`。
fn parse_ffprobe_json(json: &[u8]) -> Result<VideoInfo> {
    // 单遍流式解析：`serde_json` 直接消费字节流，未知字段被跳过，不构建 `Value` 中间树，避免每个字符串的 `String` 分配。
    let parsed: FfprobeOutput = serde_json::from_slice(json).context("解析 ffprobe JSON 输出失败")?;

    let stream = parsed.streams.into_iter().next().ok_or_else(|| anyhow!("ffprobe 未返回任何视频流"))?;

    // 拒绝 `<= 0`：损坏的容器或图像序列可能输出 `width: 0`，若不校验会静默产生错误的 `Horizontal` 方向
    let width = stream.width.filter(|w| *w > 0).ok_or_else(|| anyhow!("视频流中缺少有效的 width 字段"))?;
    let height = stream.height.filter(|h| *h > 0).ok_or_else(|| anyhow!("视频流中缺少有效的 height 字段"))?;
    let orientation = if width >= height { Orientation::Horizontal } else { Orientation::Vertical };

    Ok(VideoInfo { width, height, orientation })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn test_parse_ffprobe_json() {
        let json = r#"{
            "streams": [
                {
                    "width": 3840,
                    "height": 2160
                }
            ]
        }"#;
        let info = parse_ffprobe_json(json.as_bytes()).unwrap();
        assert_eq!(info, VideoInfo { width: 3840, height: 2160, orientation: Orientation::Horizontal });
    }

    #[test]
    fn test_parse_ffprobe_json_missing_streams() {
        let json = r"{}";
        assert!(parse_ffprobe_json(json.as_bytes()).is_err());
    }
}
