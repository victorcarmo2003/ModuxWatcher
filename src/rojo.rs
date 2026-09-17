
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Side {
    Client,
    Server,
    Shared,
}

impl Side {
    pub fn name(self) -> &'static str {
        match self {
            Side::Client => "client",
            Side::Server => "server",
            Side::Shared => "shared",
        }
    }

    pub fn sees(self, other: Side) -> bool {
        match (self, other) {
            (_, Side::Shared) => true,
            (Side::Shared, _) => false,
            (a, b) => a == b,
        }
    }
}

const SERVER_ONLY: [&str; 2] = ["ServerScriptService", "ServerStorage"];
const CLIENT_ONLY: [&str; 4] = ["StarterPlayer", "StarterGui", "StarterPack", "ReplicatedFirst"];

pub struct Map {
    entries: BTreeMap<String, Vec<String>>,
}

impl Map {
    pub fn read(project: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(project)
            .with_context(|| format!("{} not found", project.display()))?;
        let json: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", project.display()))?;
        let tree = json
            .get("tree")
            .with_context(|| format!("{} has no `tree`", project.display()))?;

        let mut entries = BTreeMap::new();
        if let Value::Object(map) = tree {
            for (key, child) in map {
                if !key.starts_with('$') {
                    descend(child, &[key.clone()], &mut entries);
                }
            }
        }
        Ok(Self { entries })
    }

    pub fn path(&self, file: &Path) -> Result<String> {
        let rel = file.to_string_lossy().replace('\\', "/");

        let mut keys: Vec<&String> = self.entries.keys().collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k.len()));

        for disk in keys {
            if rel != *disk && !rel.starts_with(&format!("{disk}/")) {
                continue;
            }
            let remainder = rel[disk.len()..].trim_matches('/');
            let mut parts: Vec<String> =
                remainder.split('/').filter(|p| !p.is_empty()).map(str::to_string).collect();

            if let Some(last) = parts.last().cloned() {
                if let Some(name) = last.strip_suffix(".luau") {
                    parts.pop();
                    if name != "init" {
                        parts.push(name.to_string());
                    }
                }
            }

            let mut trail = self.entries[disk].clone();
            trail.extend(parts);
            return Ok(trail.join("."));
        }

        bail!("path outside default.project.json: {rel}")
    }

    pub fn side(&self, file: &Path) -> Result<Side> {
        let path = self.path(file)?;
        let root = path.split('.').next().unwrap_or_default();
        Ok(if SERVER_ONLY.contains(&root) {
            Side::Server
        } else if CLIENT_ONLY.contains(&root) {
            Side::Client
        } else {
            Side::Shared
        })
    }
}

fn descend(node: &Value, trail: &[String], out: &mut BTreeMap<String, Vec<String>>) {
    let Value::Object(map) = node else { return };

    if let Some(path) = map.get("$path") {
        let text = match path {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o
                .get("optional")
                .or_else(|| o.get("path"))
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        };
        if let Some(t) = text {
            out.insert(t.replace('\\', "/").trim_end_matches('/').to_string(), trail.to_vec());
        }
    }

    for (key, child) in map {
        if key.starts_with('$') {
            continue;
        }
        let mut below = trail.to_vec();
        below.push(key.clone());
        descend(child, &below, out);
    }
}
