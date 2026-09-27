use std::collections::HashMap;

use crate::tools::{local::WebSearch, tool::Tool};

pub mod local;
pub mod tool;

pub type ToolHashMap = HashMap<String, Box<dyn Tool>>;

/// 启动时构建工具表（目前只注册本地工具）。
pub async fn build_tools() -> anyhow::Result<ToolHashMap> {
    let mut registry = ToolHashMap::new();
    insert_tools(&mut registry, local_tools());
    Ok(registry)
}

fn local_tools() -> Vec<Box<dyn Tool>> {
    vec![Box::new(WebSearch)]
}

/// 先到先得：重名工具跳过并告警。
fn insert_tools(registry: &mut ToolHashMap, tools: Vec<Box<dyn Tool>>) {
    for tool in tools {
        let name = tool.name().to_owned();
        if registry.contains_key(&name) {
            tracing::warn!("工具名 `{name}` 冲突，跳过重复注册");
            continue;
        }
        registry.insert(name, tool);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    struct DummyTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "测试用假工具"
        }

        fn parameters(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }

        async fn execute(&self, _args_json: &str) -> anyhow::Result<String> {
            Ok("ok".to_owned())
        }
    }

    #[test]
    fn duplicate_tool_names_keep_the_first() {
        let mut registry = ToolHashMap::new();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(DummyTool("x")), Box::new(DummyTool("x"))];

        insert_tools(&mut registry, tools);

        assert_eq!(registry.len(), 1, "重名工具应只保留一个");
        assert_eq!(
            registry.get("x").map(|tool| tool.description()),
            Some("测试用假工具")
        );
    }

    #[tokio::test]
    async fn builds_local_tools_only() {
        let registry = build_tools().await.expect("构建工具表应成功");
        assert!(registry.contains_key("web_search"), "应注册本地 web_search");
        assert_eq!(registry.len(), 1);
    }
}
