//! 分配式锚点账本。
//!
//! 给每个文件的每一行分配一个稳定、唯一的 4 字母锚点，并记录「模型看过哪些行」，
//! 用于编辑时的行级冲突检测。设计要点：
//!
//! - **行号 ↔ 锚点双射**：`FileAnchors.anchors` 按行序排列，下标即行号。
//! - **分配**：槽位 = `fingerprint % ANCHOR_SPACE`，冲突时按 `PROBE_STRIDE` 线性探测；
//!   同一文件内锚点唯一。
//! - **内容是否变化**由完整行指纹（[`crate::utils::hash::line_fingerprint`]）判断，
//!   不用锚点、也不用整文件哈希。
//! - **服务记录**（`served`）保存「锚点 → 展示给模型时的行指纹」；编辑时逐行比对。
//! - 一期纯内存，不持久化，进程结束即丢失。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::utils::anchor::{ANCHOR_SPACE, PROBE_STRIDE, decode_anchor, encode_anchor};

/// 默认保留的文件数上限，超出后按 LRU 回收最旧的文件。
pub const DEFAULT_FILE_CAP: usize = 256;

/// 跨工具共享的账本句柄。
pub type SharedAnchors = Arc<Mutex<AnchorRegistry>>;

/// 一次编辑命中的区间，半开区间 `[start, end)`；`replacement` 是替换后的行数。
///
/// 纯插入：`start == end`；纯删除：`replacement == 0`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditSpan {
    pub start: usize,
    pub end: usize,
    pub replacement: usize,
}

/// 单个文件的行序锚点状态。
#[derive(Debug, Default, Clone)]
pub struct FileAnchors {
    /// 行序：下标 = 行号（0-based）。
    anchors: Vec<String>,
    /// 行序：完整行指纹。
    fingerprints: Vec<u64>,
    /// 开放寻址占用表：槽位 -> 指纹。
    slots: HashMap<u64, u64>,
}

impl FileAnchors {
    /// 当前行序的锚点数组。
    pub fn anchors(&self) -> &[String] {
        &self.anchors
    }

    /// 当前行序的指纹数组。
    pub fn fingerprints(&self) -> &[u64] {
        &self.fingerprints
    }
}

/// 进程内的锚点账本。
#[derive(Default)]
pub struct AnchorRegistry {
    files: HashMap<PathBuf, FileAnchors>,
    served: HashMap<PathBuf, HashMap<String, u64>>,
    lru: VecDeque<PathBuf>,
    cap: usize,
}

impl AnchorRegistry {
    pub fn new(cap: usize) -> Self {
        Self {
            files: HashMap::new(),
            served: HashMap::new(),
            lru: VecDeque::new(),
            cap,
        }
    }

    /// 该路径是否已有锚点状态。
    pub fn contains(&self, path: &Path) -> bool {
        self.files.contains_key(path)
    }

    /// 该路径当前行序的锚点数组。
    pub fn anchors_of(&self, path: &Path) -> Option<&[String]> {
        self.files.get(path).map(|file| file.anchors.as_slice())
    }

    /// 该路径是否有可用的服务记录（模型至少看过一行）。
    pub fn has_served(&self, path: &Path) -> bool {
        self.served
            .get(path)
            .is_some_and(|served| !served.is_empty())
    }

    /// 读 / 外部变化：用前后缀对齐复用锚点，中间重新分配。
    ///
    /// 返回与 `fingerprints` 等长的锚点数组。
    pub fn align(&mut self, path: &Path, fingerprints: &[u64]) -> Vec<String> {
        let mut file = self.files.remove(path).unwrap_or_default();
        let mut served = self.served.remove(path).unwrap_or_default();

        let (prefix, suffix) = common_prefix_suffix(&file.fingerprints, fingerprints);

        let mut anchors: Vec<String> = Vec::with_capacity(fingerprints.len());
        anchors.extend(file.anchors[..prefix].iter().cloned());

        // 中间旧锚点全部释放。
        for index in prefix..(file.fingerprints.len() - suffix) {
            free_anchor(&mut file, &mut served, index);
        }

        // 中间逐行重新分配。
        let middle_end = fingerprints.len() - suffix;
        for &fingerprint in &fingerprints[prefix..middle_end] {
            anchors.push(mint(&mut file.slots, fingerprint));
        }

        // 后缀复用。
        let old_suffix_start = file.fingerprints.len() - suffix;
        anchors.extend(file.anchors[old_suffix_start..].iter().cloned());

        file.anchors = anchors.clone();
        file.fingerprints = fingerprints.to_vec();

        self.files.insert(path.to_path_buf(), file);
        self.served.insert(path.to_path_buf(), served);
        self.touch(path);
        anchors
    }

    /// 编辑：已知 [`EditSpan`] 时精确对齐，范围外绝不改动锚点。
    pub fn align_with_span(
        &mut self,
        path: &Path,
        fingerprints: &[u64],
        span: EditSpan,
    ) -> Vec<String> {
        let mut file = self.files.remove(path).unwrap_or_default();
        let mut served = self.served.remove(path).unwrap_or_default();

        let span_len = span.end.saturating_sub(span.start);

        // 1) 决定替换区每个位置是否复用旧锚点（位置存活且指纹相同）。
        let mut settled: Vec<Option<String>> = vec![None; span.replacement];
        for (offset, slot) in settled.iter_mut().enumerate() {
            if offset < span_len {
                let old = span.start + offset;
                if file.fingerprints[old] == fingerprints[span.start + offset] {
                    *slot = Some(file.anchors[old].clone());
                }
            }
        }

        // 2) 释放未被复用的旧锚点。
        let reused: HashSet<&str> = settled.iter().flatten().map(String::as_str).collect();
        for index in span.start..span.end {
            if !reused.contains(file.anchors[index].as_str()) {
                free_anchor(&mut file, &mut served, index);
            }
        }

        // 3) 其余位置分配新锚点。
        for (offset, slot) in settled.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(mint(&mut file.slots, fingerprints[span.start + offset]));
            }
        }

        // 4) 拼接：区间前 + 替换区 + 区间后。
        let mut anchors: Vec<String> = Vec::with_capacity(fingerprints.len());
        anchors.extend(file.anchors[..span.start].iter().cloned());
        anchors.extend(
            settled
                .into_iter()
                .map(|anchor| anchor.expect("已全部填充")),
        );
        anchors.extend(file.anchors[span.end..].iter().cloned());

        file.anchors = anchors.clone();
        file.fingerprints = fingerprints.to_vec();

        self.files.insert(path.to_path_buf(), file);
        self.served.insert(path.to_path_buf(), served);
        self.touch(path);
        anchors
    }

    /// 登记「模型看过」的行；顺带清理已释放的锚点。
    ///
    /// `shown` 是本次展示的行下标（0-based），只登记这些行。
    pub fn record_served(
        &mut self,
        path: &Path,
        anchors: &[String],
        fingerprints: &[u64],
        shown: &[usize],
    ) {
        let valid: HashSet<&str> = anchors.iter().map(String::as_str).collect();
        let served = self.served.entry(path.to_path_buf()).or_default();
        served.retain(|anchor, _| valid.contains(anchor.as_str()));
        for &index in shown {
            served.insert(anchors[index].clone(), fingerprints[index]);
        }
    }

    /// 逐行校验 `[from, to]`（闭区间）；不一致的行下标以 `Err` 返回。
    ///
    /// `deletion` 为真时只校验首尾两行（中间行按当前磁盘内容删除）。
    pub fn verify_range(
        &self,
        path: &Path,
        anchors: &[String],
        fingerprints: &[u64],
        from: usize,
        to: usize,
        deletion: bool,
    ) -> Result<(), Vec<usize>> {
        let served = self.served.get(path);
        let mut mismatched: Vec<usize> = Vec::new();

        for index in from..=to {
            if deletion && index != from && index != to {
                continue;
            }
            let expected = served.and_then(|map| map.get(&anchors[index]));
            if expected != Some(&fingerprints[index]) {
                mismatched.push(index);
            }
        }

        if mismatched.is_empty() {
            Ok(())
        } else {
            Err(mismatched)
        }
    }

    /// 文件被整体改写 / 删除时作废其状态。
    pub fn invalidate(&mut self, path: &Path) {
        self.files.remove(path);
        self.served.remove(path);
        self.lru.retain(|tracked| tracked != path);
    }

    /// LRU：更新访问顺序，并回收超限的最旧文件。
    fn touch(&mut self, path: &Path) {
        self.lru.retain(|tracked| tracked != path);
        self.lru.push_back(path.to_path_buf());
        while self.lru.len() > self.cap {
            if let Some(oldest) = self.lru.pop_front() {
                self.files.remove(&oldest);
                self.served.remove(&oldest);
            }
        }
    }
}

/// 最长公共前缀与后缀（按指纹比较）；`prefix + suffix <= min(两长度)`。
fn common_prefix_suffix(old: &[u64], new: &[u64]) -> (usize, usize) {
    let min = old.len().min(new.len());

    let mut prefix = 0;
    while prefix < min && old[prefix] == new[prefix] {
        prefix += 1;
    }

    let mut suffix = 0;
    while suffix < min - prefix && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix] {
        suffix += 1;
    }

    (prefix, suffix)
}

/// 释放某一行旧锚点对 `slots` 与 `served` 的占用。
fn free_anchor(file: &mut FileAnchors, served: &mut HashMap<String, u64>, line_index: usize) {
    let anchor = file.anchors[line_index].clone();
    if let Some(slot) = decode_anchor(&anchor) {
        file.slots.remove(&(slot as u64));
    }
    served.remove(&anchor);
}

/// 开放寻址 + 固定步长线性探测，分配一个空闲槽位并编码成锚点。
fn mint(slots: &mut HashMap<u64, u64>, fingerprint: u64) -> String {
    let mut slot = (fingerprint % ANCHOR_SPACE as u64) as usize;
    while slots.contains_key(&(slot as u64)) {
        slot = (slot + PROBE_STRIDE) % ANCHOR_SPACE;
    }
    slots.insert(slot as u64, fingerprint);
    encode_anchor(slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "/tmp/anchor-registry-test.txt";

    fn path() -> &'static Path {
        Path::new(P)
    }

    #[test]
    fn assign_unique_anchors_for_duplicate_fingerprints() {
        let mut registry = AnchorRegistry::new(16);
        let anchors = registry.align(path(), &[7, 7, 7]);
        assert_eq!(anchors.len(), 3);
        let unique: HashSet<&String> = anchors.iter().collect();
        assert_eq!(unique.len(), 3, "重复指纹也应拿到不同锚点");
    }

    #[test]
    fn align_keeps_prefix_and_suffix_anchors() {
        let mut registry = AnchorRegistry::new(16);
        let before = registry.align(path(), &[1, 2, 3, 4]);

        // 中间插入一行：前后缀之外的锚点应保持不变。
        let after = registry.align(path(), &[1, 9, 2, 3, 4]);
        assert_eq!(after[0], before[0]);
        assert_eq!(after[2], before[1]);
        assert_eq!(after[3], before[2]);
        assert_eq!(after[4], before[3]);

        // 删除中间一行：剩余行锚点不变。
        let after = registry.align(path(), &[1, 3, 4]);
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1], before[2]);
        assert_eq!(after[2], before[3]);
    }

    #[test]
    fn align_reassigns_only_changed_line() {
        let mut registry = AnchorRegistry::new(16);
        let before = registry.align(path(), &[1, 2, 3]);
        let after = registry.align(path(), &[1, 99, 3]);
        assert_eq!(after[0], before[0]);
        assert_eq!(after[2], before[2]);
        assert_ne!(after[1], before[1], "内容变化的行应换锚点");
    }

    #[test]
    fn align_with_span_keeps_outside_anchors() {
        let mut registry = AnchorRegistry::new(16);
        let before = registry.align(path(), &[1, 2, 3, 4]);

        // 替换第 2 行（半开区间 [1, 2)），只该行换锚点。
        let after = registry.align_with_span(
            path(),
            &[1, 99, 3, 4],
            EditSpan {
                start: 1,
                end: 2,
                replacement: 1,
            },
        );
        assert_eq!(after[0], before[0]);
        assert_ne!(after[1], before[1]);
        assert_eq!(after[2], before[2]);
        assert_eq!(after[3], before[3]);
    }

    #[test]
    fn align_with_span_supports_pure_insertion() {
        let mut registry = AnchorRegistry::new(16);
        let before = registry.align(path(), &[1, 2, 3]);

        // 在第 2 行前插入两行：[1, 2, 3) -> [1, 9, 8, 2, 3)。
        let after = registry.align_with_span(
            path(),
            &[1, 9, 8, 2, 3],
            EditSpan {
                start: 1,
                end: 1,
                replacement: 2,
            },
        );
        assert_eq!(after[0], before[0]);
        assert_eq!(after[3], before[1]);
        assert_eq!(after[4], before[2]);
    }

    #[test]
    fn align_with_span_supports_pure_deletion() {
        let mut registry = AnchorRegistry::new(16);
        let before = registry.align(path(), &[1, 2, 3]);

        // 删除第 2 行：[1, 2, 3) -> [1, 3)。
        let after = registry.align_with_span(
            path(),
            &[1, 3],
            EditSpan {
                start: 1,
                end: 2,
                replacement: 0,
            },
        );
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1], before[2]);
    }

    #[test]
    fn served_record_and_verify_range() {
        let mut registry = AnchorRegistry::new(16);
        let fingerprints = [11u64, 22, 33];
        let anchors = registry.align(path(), &fingerprints);
        registry.record_served(path(), &anchors, &fingerprints, &[0, 1, 2]);

        // 全部未变。
        assert!(
            registry
                .verify_range(path(), &anchors, &fingerprints, 0, 2, false)
                .is_ok()
        );

        // 第 2 行内容变了（指纹不同，anchor 仍是旧值）。
        let changed = [11u64, 999, 33];
        let mismatch = registry
            .verify_range(path(), &anchors, &changed, 0, 2, false)
            .expect_err("第 2 行应报冲突");
        assert_eq!(mismatch, vec![1]);
    }

    #[test]
    fn verify_range_rejects_unserved_lines() {
        let mut registry = AnchorRegistry::new(16);
        let fingerprints = [11u64, 22, 33];
        let anchors = registry.align(path(), &fingerprints);
        // 只展示第 1、2 行；第 3 行从未展示。
        registry.record_served(path(), &anchors, &fingerprints, &[0, 1]);

        assert!(
            registry
                .verify_range(path(), &anchors, &fingerprints, 2, 2, false)
                .is_err()
        );

        // 纯删除只看首尾：尾行未展示，因此仍然报冲突。
        assert!(
            registry
                .verify_range(path(), &anchors, &fingerprints, 0, 2, true)
                .is_err()
        );
    }

    #[test]
    fn invalidate_and_lru_cap() {
        let mut registry = AnchorRegistry::new(1);
        registry.align(Path::new("/tmp/a.txt"), &[1, 2]);
        registry.align(Path::new("/tmp/b.txt"), &[3, 4]);
        assert!(
            !registry.contains(Path::new("/tmp/a.txt")),
            "超过 cap 后最旧文件应被回收"
        );

        registry.invalidate(Path::new("/tmp/b.txt"));
        assert!(!registry.contains(Path::new("/tmp/b.txt")));
    }
}
