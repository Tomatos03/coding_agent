//! 危险工具审批策略：哪些工具在执行前需要人工确认。
//!
//! 配置读自**工作区根目录**（即进程当前目录）下的 `.agents/settings.json`；读取不到时
//! 返回全默认配置（全放行），与没有这道闸门时行为一致。本模块只回答「哪些工具要问」，
//! 问谁、怎么问由 [`crate::agent::react::approval::Confirmer`] 决定。

use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

/// 设置文件路径，相对工作区根目录（即进程当前目录）。
pub const SETTINGS_PATH: &str = ".agents/settings.json";

/// 顶层配置。`deny_unknown_fields` 让字段名拼错在启动时立即报错，而不是被静默忽略。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub approval: ApprovalPolicy,
}

/// 工具审批策略：规则自上而下、**第一条命中即生效**，全部未命中用 `default_action`。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct ApprovalPolicy {
    pub default_action: ApprovalAction,
    pub rules: Vec<ApprovalRule>,
}

/// 审批动作。v1 只有「放行」与「询问」，`deny` 是预留的扩展位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalAction {
    #[default]
    Allow,
    Ask,
}

/// 一条规则。`pattern` 匹配工具暴露名（如 `filesystem__write_file`、`web_search`）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRule {
    pub pattern: String,
    pub action: ApprovalAction,
}

impl ApprovalPolicy {
    /// 查出某个工具该走哪个动作。纯查表，与工具实现、会话状态无关。
    pub fn action_for(&self, tool: &str) -> ApprovalAction {
        self.rules
            .iter()
            .find(|rule| glob_match(&rule.pattern, tool))
            .map(|rule| rule.action)
            .unwrap_or(self.default_action)
    }
}

/// 只支持 `*` 的单星 glob：`*` 匹配任意字符序列（含空），全名锚定、大小写敏感。
fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();

    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut resume = 0usize;

    while n < name.len() {
        if p < pattern.len() && pattern[p] == name[n] {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = n;
            p += 1;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            resume += 1;
            n = resume;
        } else {
            return false;
        }
    }

    pattern[p..].iter().all(|c| *c == '*')
}

/// 解析并校验配置文本。纯函数，便于单元测试。
pub fn parse_settings(raw: &str) -> anyhow::Result<Settings> {
    let settings: Settings = serde_json::from_str(raw).context("解析 settings JSON 失败")?;

    for rule in &settings.approval.rules {
        if rule.pattern.trim().is_empty() {
            anyhow::bail!("approval 规则的 pattern 不能为空");
        }
    }

    Ok(settings)
}

/// 从指定路径加载配置。文件不存在视为「未配置审批」，返回全默认（全放行）。
pub fn load_settings_from(path: &Path) -> anyhow::Result<Settings> {
    if !path.exists() {
        tracing::info!(
            "未找到设置文件 {}，审批策略使用默认值（全放行）",
            path.display()
        );
        return Ok(Settings::default());
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("读取设置文件失败: {}", path.display()))?;

    parse_settings(&raw).with_context(|| format!("加载设置文件失败: {}", path.display()))
}

/// 从工作区根目录的 [`SETTINGS_PATH`] 加载配置。
pub fn load_settings() -> anyhow::Result<Settings> {
    load_settings_from(Path::new(SETTINGS_PATH))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{
        "approval": {
            "defaultAction": "allow",
            "rules": [
                { "pattern": "*__*", "action": "ask" },
                { "pattern": "web_search", "action": "allow" }
            ]
        }
    }"#;

    fn policy(default_action: ApprovalAction, rules: &[(&str, ApprovalAction)]) -> ApprovalPolicy {
        ApprovalPolicy {
            default_action,
            rules: rules
                .iter()
                .map(|(pattern, action)| ApprovalRule {
                    pattern: (*pattern).to_owned(),
                    action: *action,
                })
                .collect(),
        }
    }

    #[test]
    fn parses_full_config() {
        let settings = parse_settings(FULL).expect("完整配置应解析成功");

        assert_eq!(settings.approval.default_action, ApprovalAction::Allow);
        assert_eq!(settings.approval.rules.len(), 2);
        assert_eq!(settings.approval.rules[0].pattern, "*__*");
        assert_eq!(settings.approval.rules[0].action, ApprovalAction::Ask);
        assert_eq!(settings.approval.rules[1].action, ApprovalAction::Allow);
    }

    #[test]
    fn empty_object_yields_defaults() {
        let settings = parse_settings("{}").expect("空对象应可用");

        assert_eq!(settings.approval.default_action, ApprovalAction::Allow);
        assert!(settings.approval.rules.is_empty());
    }

    #[test]
    fn partial_approval_yields_defaults() {
        let settings =
            parse_settings(r#"{"approval":{"rules":[{"pattern":"echo","action":"ask"}]}}"#)
                .expect("缺省字段应回退到默认值");

        assert_eq!(settings.approval.default_action, ApprovalAction::Allow);
        assert_eq!(settings.approval.rules.len(), 1);
    }

    #[test]
    fn invalid_action_is_rejected() {
        let err = parse_settings(r#"{"approval":{"rules":[{"pattern":"echo","action":"maybe"}]}}"#)
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("maybe"),
            "错误信息应提到非法值的来源: {err:#}"
        );
    }

    #[test]
    fn blank_pattern_is_rejected() {
        let err = parse_settings(r#"{"approval":{"rules":[{"pattern":"  ","action":"ask"}]}}"#)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("pattern"),
            "错误信息应提到 pattern: {err:#}"
        );

        assert!(
            parse_settings(r#"{"approval":{"rules":[{"pattern":"","action":"ask"}]}}"#).is_err()
        );
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(
            parse_settings(r#"{"unknown":1}"#).is_err(),
            "顶层未知字段应报错"
        );
        assert!(
            parse_settings(r#"{"approval":{"rules":[{"pattern":"echo","action":"ask","x":1}]}}"#)
                .is_err(),
            "规则级未知字段应报错"
        );
    }

    #[test]
    fn missing_file_yields_default_config() {
        let path = Path::new("__settings_should_not_exist__.json");
        let settings = load_settings_from(path).expect("文件不存在不应报错");

        assert_eq!(
            settings.approval.action_for("delete_files"),
            ApprovalAction::Allow
        );
        assert!(settings.approval.rules.is_empty());
    }

    #[test]
    fn glob_matches_named_shapes() {
        let policy = policy(
            ApprovalAction::Allow,
            &[
                ("web_search", ApprovalAction::Ask),
                ("filesystem__*", ApprovalAction::Ask),
                ("*__echo", ApprovalAction::Ask),
            ],
        );

        assert_eq!(policy.action_for("web_search"), ApprovalAction::Ask);
        assert_eq!(
            policy.action_for("web_search_extra"),
            ApprovalAction::Allow,
            "全名锚定：更长的名字不应被精确规则命中"
        );
        assert_eq!(
            policy.action_for("filesystem__write_file"),
            ApprovalAction::Ask
        );
        assert_eq!(policy.action_for("probe__echo"), ApprovalAction::Ask);
        assert_eq!(
            policy.action_for("probe__echo_extra"),
            ApprovalAction::Allow,
            "后缀通配不吸尾"
        );
        assert_eq!(
            policy.action_for("echo"),
            ApprovalAction::Allow,
            "`*__echo` 要求名字里出现 `__`"
        );
    }

    #[test]
    fn bare_star_matches_everything() {
        let policy = policy(ApprovalAction::Allow, &[("*", ApprovalAction::Ask)]);

        assert_eq!(policy.action_for("anything"), ApprovalAction::Ask);
        assert_eq!(policy.action_for(""), ApprovalAction::Ask);
    }

    #[test]
    fn first_matching_rule_wins() {
        let policy = policy(
            ApprovalAction::Allow,
            &[
                ("*", ApprovalAction::Ask),
                ("web_search", ApprovalAction::Allow),
            ],
        );

        assert_eq!(
            policy.action_for("web_search"),
            ApprovalAction::Ask,
            "首条命中即生效，后面的规则不再参与"
        );
    }

    #[test]
    fn default_action_applies_when_no_rule_matches() {
        let policy = policy(
            ApprovalAction::Ask,
            &[("web_search", ApprovalAction::Allow)],
        );

        assert_eq!(policy.action_for("web_search"), ApprovalAction::Allow);
        assert_eq!(policy.action_for("delete_files"), ApprovalAction::Ask);
    }
}
