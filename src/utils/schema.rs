//! JSON Schema 后处理：把 `schemars` 的输出修成 OpenAI function parameters 能接受的形状。
//!
//! 两件事：
//! 1. 压平「内部标签枚举」的顶层 `oneOf`（[`flatten_tagged_union`]）；
//! 2. 收敛 `Option<T>` 带来的 `"type": ["X", "null"]`（[`collapse_nullable_types`]）。
//!
//! 纯函数：输入 schema，输出 schema，不碰任何工具状态。

use serde_json::{Map, Value, json};

/// 发给服务端前的完整归一化：先压平 tagged union，再收敛 nullable 联合。
///
/// `Tool::definition()` 是唯一调用方——所有工具（含 MCP）都从这里过。
pub fn normalize_parameters(schema: Value) -> Value {
    collapse_nullable_types(flatten_tagged_union(schema))
}

/// 压平顶层 `oneOf`，变成单层 object schema。
///
/// **保守**：只有当每个分支都是带 `properties` 的 object 时才动手；否则原样返回
/// （服务端本来也会拒绝，但至少不会因为我们的合并而改变语义）。
///
/// 合并规则：
/// - `properties` 取并集；同名属性两边都是 `const`／`enum` 时求并集 —— 这正是内部标签
///   （`command`）的形态，合并后成为它的取值枚举；
/// - `required` 取**交集**：只在部分分支里必填的字段降级为可选。单层 schema 表达不了
///   「按 `command` 分支必填」，缺字段由运行期反序列化报错、再由 ReAct 循环压成
///   Observation 让模型自己纠正；
/// - 其余顶层字段（`$schema` / `title` 等）原样保留。
pub fn flatten_tagged_union(schema: Value) -> Value {
    let Value::Object(mut root) = schema else {
        return schema;
    };

    let Some(Value::Array(variants)) = root.get("oneOf").cloned() else {
        return Value::Object(root);
    };
    let Some(flat) = flatten_variants(&variants) else {
        return Value::Object(root);
    };

    root.remove("oneOf");
    root.insert("type".to_owned(), json!("object"));
    root.insert("properties".to_owned(), Value::Object(flat.properties));
    if !flat.required.is_empty() {
        root.insert("required".to_owned(), json!(flat.required));
    }

    Value::Object(root)
}

struct FlatSchema {
    properties: Map<String, Value>,
    required: Vec<String>,
}

/// 把 `"type": ["X", "null"]` 收敛成 `"type": "X"`。
///
/// `Option<T>` 经 `schemars` 出来就是这种联合；这些字段本来就**可选**（不在 `required`
/// 里），模型完全可以省略，而 OpenAI 的 function parameters 对 union type 支持不稳。
/// 收敛不改变运行期行为：`Option<T>` 对「缺字段」与「显式 null」都能反序列化。
///
/// **保守**：只有「去掉 `null` 后恰好剩一个类型」时才改；多类型联合原样保留。
pub fn collapse_nullable_types(schema: Value) -> Value {
    let mut schema = schema;
    collapse_nullable(&mut schema);
    schema
}

fn collapse_nullable(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get("type") {
                let kept: Vec<Value> = types
                    .iter()
                    .filter(|entry| entry.as_str() != Some("null"))
                    .cloned()
                    .collect();
                if kept.len() == 1 && kept.len() < types.len() {
                    map.insert("type".to_owned(), kept[0].clone());
                }
            }
            for nested in map.values_mut() {
                collapse_nullable(nested);
            }
        }
        Value::Array(items) => {
            for item in items {
                collapse_nullable(item);
            }
        }
        _ => {}
    }
}

/// 所有分支都是带 `properties` 的 object 时，合并出单层 schema；否则 `None`。
fn flatten_variants(variants: &[Value]) -> Option<FlatSchema> {
    let mut properties = Map::new();
    let mut required: Option<Vec<String>> = None;

    for variant in variants {
        let Value::Object(variant) = variant else {
            return None;
        };
        let Some(Value::Object(branch)) = variant.get("properties") else {
            return None;
        };

        for (name, property) in branch {
            match properties.get_mut(name) {
                Some(existing) => merge_property(existing, property),
                None => {
                    properties.insert(name.clone(), property.clone());
                }
            }
        }

        let names = required_names(variant);
        required = Some(match required {
            None => names,
            Some(current) => current
                .into_iter()
                .filter(|name| names.contains(name))
                .collect(),
        });
    }

    Some(FlatSchema {
        properties,
        required: required.unwrap_or_default(),
    })
}

fn required_names(variant: &Map<String, Value>) -> Vec<String> {
    variant
        .get("required")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// 同名属性合并：两边都是 `const`／`enum` 时求并集，其余情况保留先到的那个
/// （同名字段在各分支上的描述通常一致）。
fn merge_property(existing: &mut Value, incoming: &Value) {
    let (Some(mut values), Some(more)) = (constants(existing), constants(incoming)) else {
        return;
    };
    for value in more {
        if !values.contains(&value) {
            values.push(value);
        }
    }
    if let Value::Object(map) = existing {
        map.remove("const");
        map.insert("enum".to_owned(), Value::Array(values));
    }
}

/// 取出属性上的常量集合：`const` 视作单元素，`enum` 原样。
fn constants(value: &Value) -> Option<Vec<Value>> {
    let Value::Object(map) = value else {
        return None;
    };
    if let Some(one) = map.get("const") {
        return Some(vec![one.clone()]);
    }
    match map.get("enum") {
        Some(Value::Array(items)) => Some(items.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `edit_file` 的缩略版：三个分支，各有自己的必填字段。
    fn tagged_union() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "EditFileArgs",
            "oneOf": [
                {
                    "description": "精确字符串替换。",
                    "type": "object",
                    "properties": {
                        "command": { "const": "str_replace", "type": "string" },
                        "path": { "type": "string" },
                        "old_string": { "type": "string" },
                        "new_string": { "type": "string" },
                        "replace_all": { "type": ["boolean", "null"] }
                    },
                    "required": ["command", "path", "old_string", "new_string"]
                },
                {
                    "description": "在指定行之后插入文本。",
                    "type": "object",
                    "properties": {
                        "command": { "const": "insert", "type": "string" },
                        "path": { "type": "string" },
                        "insert_line": { "type": "integer" },
                        "insert_text": { "type": "string" }
                    },
                    "required": ["command", "path", "insert_line", "insert_text"]
                }
            ]
        })
    }

    #[test]
    fn flattens_into_a_single_object_schema() {
        let flat = flatten_tagged_union(tagged_union());

        assert_eq!(flat["type"], "object");
        assert!(flat["oneOf"].is_null(), "oneOf 应被移除");
        assert!(flat["properties"].is_object());
        assert_eq!(flat["title"], "EditFileArgs", "元信息应保留");

        // 标签字段合并成取值枚举。
        assert_eq!(
            flat["properties"]["command"]["enum"],
            json!(["str_replace", "insert"])
        );
        assert!(flat["properties"]["command"]["const"].is_null());

        // 并集：两个分支的字段都在。
        for name in [
            "path",
            "old_string",
            "new_string",
            "insert_line",
            "insert_text",
        ] {
            assert!(
                flat["properties"].get(name).is_some(),
                "缺少字段 `{name}`：{flat}"
            );
        }

        // 只有每个分支都必填的字段才留在 required。
        assert_eq!(flat["required"], json!(["command", "path"]));
    }

    #[test]
    fn keeps_plain_object_schemas_untouched() {
        let plain = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        });

        assert_eq!(flatten_tagged_union(plain.clone()), plain);
    }

    #[test]
    fn leaves_unions_of_non_objects_untouched() {
        let union = json!({ "oneOf": [{ "type": "string" }, { "type": "integer" }] });

        assert_eq!(flatten_tagged_union(union.clone()), union, "保守：不动它");
    }

    #[test]
    fn collapses_nullable_unions_only_when_one_type_remains() {
        let schema = json!({
            "type": "object",
            "properties": {
                "replace_all": { "type": ["boolean", "null"] },
                "both": { "type": ["string", "null"] },
                "many": { "type": ["string", "integer", "null"] },
                "already_plain": { "type": "string" },
                "nested": { "items": { "type": ["string", "null"] }, "type": "array" }
            }
        });

        let collapsed = collapse_nullable_types(schema);
        let properties = &collapsed["properties"];

        assert_eq!(properties["replace_all"]["type"], "boolean");
        assert_eq!(properties["both"]["type"], "string");
        assert_eq!(
            properties["many"]["type"],
            json!(["string", "integer", "null"]),
            "多类型联合保持原样"
        );
        assert_eq!(properties["already_plain"]["type"], "string");
        assert_eq!(
            properties["nested"]["items"]["type"], "string",
            "递归到嵌套结构"
        );
    }

    #[test]
    fn normalize_parameters_does_both_passes() {
        let normalized = normalize_parameters(tagged_union());

        assert_eq!(normalized["type"], "object");
        assert_eq!(
            normalized["properties"]["command"]["enum"],
            json!(["str_replace", "insert"])
        );
        assert_eq!(
            normalized["properties"]["replace_all"]["type"], "boolean",
            "压平之后也要把 nullable 收敛掉"
        );
    }
}
