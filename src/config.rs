/*
 * Copyright (c) 2026, LJason. All Rights Reserved.
 */

//! 配置文件解析模块，负责从 JSON 文件加载目录映射与 m3u 输出配置。

use std::{
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{
    Deserialize, Deserializer,
    de::{self, SeqAccess, Visitor},
};

use crate::{db::Orientation, expand_tilde};

/// 一个（或多个）本地目录与对应 m3u 输出文件的映射。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListItem {
    /// 一个或多个本地目录路径。
    ///
    /// 支持 JSON 字符串或字符串数组，反序列化后统一转换为 `Vec<PathBuf>`。
    ///
    /// **至少一个元素**：空数组会在反序列化阶段被拒绝，因为"零个目录"对下游的目录遍历无意义。
    #[serde(deserialize_with = "deserialize_path_list")]
    pub paths: Vec<PathBuf>,
    /// 生成的 m3u 播放列表文件的保存路径。
    pub m3u: PathBuf,
    /// 可选方向过滤，仅输出指定方向的视频。
    ///
    /// - 当 `Some(Orientation::Vertical)` 时，仅包含竖屏视频；
    /// - 当 `Some(Orientation::Horizontal)` 时，仅包含横屏视频；
    /// - 当 `None` 时输出所有视频。
    pub orientation: Option<Orientation>,
}

/// 将"字符串或字符串数组"反序列化为非空的 `Vec<PathBuf>`。
///
/// # Errors
/// - 输入不是字符串也不是字符串数组时，由 serde 生成包含行列位置的标准错误；
/// - 字符串数组的元素不是字符串时，错误信息包含**元素位置**；
/// - 输入为空数组时，返回语义错误（违反"至少一个路径"的契约）。
fn deserialize_path_list<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<PathBuf>, D::Error> {
    /// 单字符串或字符串数组的 [`Visitor`] 实现。
    struct PathListVisitor;

    impl<'de> Visitor<'de> for PathListVisitor {
        type Value = Vec<PathBuf>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("路径字符串或非空的路径字符串数组")
        }

        /// 处理借用的字符串输入（`serde_json::from_str` 路径）。
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![PathBuf::from(v)])
        }

        /// 处理拥有的字符串输入（`serde_json::from_reader` 路径）。
        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(vec![PathBuf::from(v)])
        }

        /// 处理数组输入；元素类型由 `PathBuf::deserialize` 校验，错误位置由 serde 自动附加。
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            // 预分配，避免扩容重分配。`size_hint` 对 JSON 数组是精确的
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(1));

            while let Some(path) = seq.next_element::<PathBuf>()? {
                out.push(path);
            }

            if out.is_empty() {
                return Err(de::Error::invalid_length(0, &"至少一条路径"));
            }

            Ok(out)
        }
    }

    deserializer.deserialize_any(PathListVisitor)
}

/// 配置文件中的单个映射配置项。
///
/// 对应 JSON 数组中的一个对象，包含远程 URL 前缀、本地路径前缀以及目录与 m3u 输出映射列表。
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// `WebDAV` 基础 URL，用于拼接生成远程播放地址。
    pub head: String,
    /// 本地路径前缀，视频文件绝对路径去掉该前缀后得到相对路径，再与 [`Config::head`] 拼接。
    pub replace: String,
    /// 该配置项包含的目录（可能多个）与对应 m3u 输出文件的映射列表。
    pub list: Vec<ListItem>,
}

impl Config {
    /// 加载并解析指定路径的配置文件。
    ///
    /// # Errors
    /// - 路径以 `~` 开头但无法解析当前用户家目录；
    /// - 文件无法打开或读取（`io::Error`）；
    /// - 文件内容不是合法 UTF-8；
    /// - JSON 语法错误或反序列化失败（`serde_json::Error`，错误信息包含行列位置）。
    pub fn from_file(path: &Path) -> Result<Vec<Self>> {
        let expanded = expand_tilde(path)?;
        let content = std::fs::read_to_string(expanded.as_ref()).with_context(|| format!("读取配置文件失败：{}", expanded.display()))?;
        let configs: Vec<Config> = serde_json::from_str(&content).with_context(|| format!("解析配置文件失败：{}", expanded.display()))?;
        Ok(configs)
    }
}
