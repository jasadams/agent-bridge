use anyhow::{Context, Result, ensure};
use jsonc_parser::{
    ParseOptions,
    cst::{CstNode, CstRootNode},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Owned {
    pub plugin: String,
    pub mcp: String,
    pub url: String,
}

pub(super) fn parse(text: &str) -> Result<CstRootNode> {
    let root = CstRootNode::parse(
        text,
        &ParseOptions {
            allow_comments: true,
            allow_trailing_commas: true,
            allow_loose_object_property_names: false,
            allow_missing_commas: false,
            allow_single_quoted_strings: false,
            allow_hexadecimal_numbers: false,
            allow_unary_plus_numbers: false,
            allow_bare_decimal_point_numbers: false,
            allow_non_finite_numbers: false,
            allow_extended_string_escapes: false,
        },
    )?;
    let node = root
        .value()
        .context("Configuration must contain an object")?;
    ensure!(
        node.as_object().is_some(),
        "Configuration must contain an object"
    );
    unique(node)?;
    Ok(root)
}
fn unique(node: CstNode) -> Result<()> {
    if let Some(object) = node.as_object() {
        let mut names = HashSet::new();
        for property in object.properties() {
            ensure!(
                names.insert(property.decoded_name().context("Invalid property name")?),
                "Duplicate configuration key"
            );
            unique(property.value().context("Missing property value")?)?;
        }
    } else if let Some(array) = node.as_array() {
        for node in array.elements() {
            unique(node)?;
        }
    }
    Ok(())
}
pub(super) fn value(root: &CstRootNode, key: &str) -> Option<Value> {
    root.value()?
        .as_object()?
        .get(key)?
        .value()?
        .to_serde_value()
}
fn comments(node: CstNode, result: &mut Vec<String>) {
    if node.as_comment().is_some() {
        result.push(node.to_string());
    }
    for child in node.children() {
        comments(child, result);
    }
}
fn comment_list(root: &CstRootNode) -> Vec<String> {
    let mut result = Vec::new();
    for child in root.children() {
        comments(child, &mut result);
    }
    result.sort();
    result
}
fn looks_managed(value: &Value, namespace: &str) -> bool {
    let spec = value
        .as_str()
        .or_else(|| value.as_array()?.first()?.as_str());
    spec.is_some_and(|s| {
        s.starts_with(namespace)
            || s.starts_with("@agent-bridge/opencode-plugin")
            || s.ends_with("/plugins/opencode/index.ts")
    })
}
pub(super) fn validate_shape(root: &CstRootNode) -> Result<()> {
    if let Some(value) = value(root, "plugin") {
        ensure!(value.is_array(), "plugin must be an array");
    }
    if let Some(value) = value(root, "mcp") {
        ensure!(value.is_object(), "mcp must be an object");
    }
    Ok(())
}
pub(super) fn conflict(root: &CstRootNode, namespace: &str) -> Result<bool> {
    validate_shape(root)?;
    Ok(
        value(root, "mcp").is_some_and(|mcp| mcp.get("agent-bridge").is_some())
            || value(root, "plugin")
                .and_then(|v| v.as_array().cloned())
                .is_some_and(|p| p.iter().any(|v| looks_managed(v, namespace))),
    )
}

pub(super) fn edit(
    text: &str,
    old: Option<&Owned>,
    new: Option<(&str, &std::path::Path, &std::path::Path)>,
    namespace: &str,
) -> Result<(String, Option<Owned>)> {
    let root = parse(text)?;
    validate_shape(&root)?;
    let initial_comments = comment_list(&root);
    let object = root.value().unwrap().as_object().unwrap();
    if let Some(old) = old {
        let plugins = object
            .get("plugin")
            .and_then(|p| p.value()?.as_array())
            .context(
                "Managed plugin array was edited; preserve it and resolve the conflict manually",
            )?;
        let matches: Vec<_> = plugins
            .elements()
            .into_iter()
            .filter(|v| v.to_string() == old.plugin)
            .collect();
        ensure!(
            matches.len() == 1,
            "Managed plugin entry was edited or duplicated; refusing to overwrite owner changes"
        );
        let mcp = object
            .get("mcp")
            .and_then(|p| p.value()?.as_object())
            .context("Managed MCP object was edited")?;
        let entry = mcp
            .get("agent-bridge")
            .context("Managed MCP entry was removed or edited")?;
        ensure!(
            entry.to_string() == old.mcp,
            "Managed MCP entry was edited; refusing to overwrite owner changes"
        );
        ensure!(
            plugins
                .elements()
                .iter()
                .filter(|node| node
                    .to_serde_value()
                    .is_some_and(|v| looks_managed(&v, namespace)))
                .count()
                == 1,
            "Another Agent Bridge plugin entry exists; resolve that conflict before updating"
        );
        if let Some((url, binary, state)) = new {
            let tuple = serde_json::json!([url, {"stateDir": state}]);
            let server = serde_json::json!({"type":"local","command":[binary,"--state-dir",state,"mcp"],"enabled":true});
            if matches[0].to_serde_value() == Some(tuple)
                && entry.value().and_then(|v| v.to_serde_value()) == Some(server)
            {
                return Ok((text.into(), Some(old.clone())));
            }
        }
        matches[0].clone().remove();
        entry.remove();
        ensure!(
            comment_list(&root) == initial_comments,
            "Removing the managed entries would remove owner comments; move those comments outside the entries before retrying"
        );
    }
    // Other copies are never adopted, even when semantically identical.
    ensure!(
        !conflict(&root, namespace)?,
        "Unowned Agent Bridge plugin or mcp.agent-bridge entry exists; preserve it and resolve the conflict manually"
    );
    let owned = if let Some((url, binary, state)) = new {
        if object.get("plugin").is_none() {
            object.append("plugin", jsonc_parser::json!([]));
        }
        if object.get("mcp").is_none() {
            object.append("mcp", jsonc_parser::json!({}));
        }
        let plugins = object
            .get("plugin")
            .unwrap()
            .value()
            .unwrap()
            .as_array()
            .unwrap();
        let tuple = serde_json::json!([url,{"stateDir":state}]);
        let node = plugins.append(input(tuple));
        let mcp = object
            .get("mcp")
            .unwrap()
            .value()
            .unwrap()
            .as_object()
            .unwrap();
        let entry = mcp.append("agent-bridge", input(serde_json::json!({"type":"local","command":[binary,"--state-dir",state,"mcp"],"enabled":true})));
        Some(Owned {
            plugin: node.to_string(),
            mcp: entry.to_string(),
            url: url.into(),
        })
    } else {
        None
    };
    ensure!(
        comment_list(&root) == initial_comments,
        "Editing would change owner comments"
    );
    let result = root.to_string();
    parse(&result)?;
    Ok((result, owned))
}

fn input(value: Value) -> jsonc_parser::cst::CstInputValue {
    use jsonc_parser::cst::CstInputValue as Input;
    match value {
        Value::Null => Input::Null,
        Value::Bool(v) => Input::Bool(v),
        Value::Number(v) => Input::Number(v.to_string()),
        Value::String(v) => Input::String(v),
        Value::Array(values) => Input::Array(values.into_iter().map(input).collect()),
        Value::Object(values) => {
            Input::Object(values.into_iter().map(|(k, v)| (k, input(v))).collect())
        }
    }
}
