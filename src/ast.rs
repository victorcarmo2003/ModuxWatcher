
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::Value;

pub const BIN: &str = "luau-ast";

pub struct Source {
    lines: Vec<String>,
}

impl Source {
    pub fn new(text: &str) -> Self {
        Self {
            lines: text.replace("\r\n", "\n").split('\n').map(str::to_string).collect(),
        }
    }

    fn point(s: &str) -> (usize, usize) {
        let mut parts = s.trim().split(',');
        let line = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let column = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        (line, column)
    }

    pub fn slice(&self, location: &str) -> String {
        let mut sides = location.split('-');
        let (l0, c0) = Self::point(sides.next().unwrap_or("0,0"));
        let (l1, c1) = Self::point(sides.next().unwrap_or("0,0"));

        let line_at = |i: usize| self.lines.get(i).map(String::as_str).unwrap_or("");
        let cut = |s: &str, start_at: usize, end_at: usize| -> String {
            let chars: Vec<char> = s.chars().collect();
            let start_at = start_at.min(chars.len());
            let end_at = end_at.min(chars.len());
            chars[start_at..end_at].iter().collect()
        };

        if l0 == l1 {
            return cut(line_at(l0), c0, c1);
        }
        let mut parts = vec![cut(line_at(l0), c0, usize::MAX)];
        for i in (l0 + 1)..l1 {
            parts.push(line_at(i).to_string());
        }
        parts.push(cut(line_at(l1), 0, c1));
        parts.join("\n")
    }
}

pub fn parse(path: &Path) -> Result<(Vec<Value>, Source)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;

    let out = Command::new(BIN).arg(path).output().with_context(|| {
        format!(
            "`{BIN}` not found in PATH.\n\
             It ships inside the luau-lang/luau release archive (luau-windows.zip,\n\
             luau-ubuntu.zip, luau-macos.zip), next to luau-analyze. `rokit add\n\
             luau-lang/luau` does NOT provide it: rokit keeps one binary per tool\n\
             and that one is `luau`. Download the archive and put luau-ast on PATH."
        )
    })?;

    if !out.status.success() {
        bail!(
            "{BIN} falhou em {}:\n{}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let json: Value = serde_json::from_slice(&out.stdout)
        .with_context(|| format!("{BIN} output is not valid JSON for {}", path.display()))?;

    let body = json
        .get("root")
        .and_then(|r| r.get("body"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    Ok((body, Source::new(&text)))
}

pub fn kind_of(node: &Value) -> &str {
    node.get("type").and_then(Value::as_str).unwrap_or("")
}

pub fn text<'a>(node: &'a Value, key: &str) -> &'a str {
    node.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn list<'a>(node: &'a Value, key: &str) -> &'a [Value] {
    node.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

pub fn is_local(node: &Value, name: &str) -> bool {
    kind_of(node) == "AstExprLocal" && node.get("local").map(|l| text(l, "name")) == Some(name)
}

pub fn indexes(node: &Value, name: &str) -> bool {
    kind_of(node) == "AstExprIndexName" && text(node, "index") == name
}

pub fn walk(node: &Value, visit: &mut dyn FnMut(&Value)) {
    match node {
        Value::Object(map) => {
            if map.contains_key("type") {
                visit(node);
            }
            for v in map.values() {
                walk(v, visit);
            }
        }
        Value::Array(items) => {
            for v in items {
                walk(v, visit);
            }
        }
        _ => {}
    }
}
