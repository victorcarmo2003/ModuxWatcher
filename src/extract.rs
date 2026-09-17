
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::Value;

use crate::ast::{self, walk, is_local, indexes, list, text, kind_of, Source};

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Method {
    pub name: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Field {
    pub name: String,
    pub ty: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Import {
    pub alias: String,
    pub expr: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Service {
    pub alias: String,
    pub service: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalType {
    pub name: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Module {
    pub id: String,
    pub kind: String,
    pub file: String,
    pub services: Vec<Service>,
    pub requires: Vec<Import>,
    pub local_types: Vec<LocalType>,
    pub methods: Vec<Method>,
    pub fields: Vec<Field>,
    pub dependencies: Vec<String>,
    pub declared_require: Vec<String>,
    pub issues: Vec<Issue>,
}

fn literal(t: &str) -> Option<&'static str> {
    match t {
        "AstExprConstantString" => Some("string"),
        "AstExprConstantNumber" => Some("number"),
        "AstExprConstantBool" => Some("boolean"),
        _ => None,
    }
}

pub struct Extractor {
    path: PathBuf,
    fonte: Source,
    root: Vec<Value>,
    issues: Vec<Issue>,
}

impl Extractor {
    pub fn new(path: &Path) -> Result<Self> {
        let (root, fonte) = ast::parse(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            fonte,
            root,
            issues: Vec::new(),
        })
    }

    fn type_text(&self, annotation: &Value) -> String {
        self.fonte.slice(text(annotation, "location")).trim().to_string()
    }

    fn issue(&mut self, node: &Value, msg: String) {
        let loc = text(node, "location");
        let started = loc.split('-').next().unwrap_or("0,0").trim();
        let mut parts = started.split(',');
        let line: usize = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let column: usize = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        self.issues.push(Issue {
            line: line + 1,
            column: column + 1,
            message: msg,
        });
    }

    fn value_type(&mut self, value: &Value, where_at: &str) -> Option<String> {
        let t = kind_of(value);
        if t == "AstExprTypeAssertion" {
            return value.get("annotation").map(|a| self.type_text(a));
        }
        if let Some(prim) = literal(t) {
            return Some(prim.to_string());
        }
        if t == "AstExprFunction" {
            return Some(self.signature(value, false));
        }
        self.issue(
            value,
            format!("{where_at} has no type. Annotate it with `::` so the generator can transcribe it."),
        );
        None
    }

    fn signature(&mut self, func: &Value, with_self: bool) -> String {
        let all_args = list(func, "args").to_vec();
        let mut parts = Vec::new();

        let args: Vec<Value> = if with_self {
            parts.push("self: Public".to_string());
            let implicit_self = func.get("self").map_or(true, Value::is_null);
            let first_arg_e_self = all_args.first().map(|a| text(a, "name")) == Some("self");
            if implicit_self && first_arg_e_self {
                all_args[1..].to_vec()
            } else {
                all_args
            }
        } else {
            all_args
        };

        for arg in &args {
            let name = text(arg, "name").to_string();
            match arg.get("luauType").filter(|v| !v.is_null()) {
                Some(anot) => parts.push(format!("{name}: {}", self.type_text(anot))),
                None => {
                    self.issue(func, format!("parametro `{name}` sem annotation de kind_of."));
                    parts.push(format!("{name}: unknown"));
                }
            }
        }

        format!("({}) -> {}", parts.join(", "), self.return_type(func))
    }

    fn return_type(&self, func: &Value) -> String {
        match func.get("returnAnnotation").filter(|v| !v.is_null()) {
            Some(ret) => self.type_text(ret),
            None => "()".to_string(),
        }
    }

    pub fn run(mut self) -> Result<Module> {
        let Some((local_name, id, declared_require, kind)) = self.declaration() else {
            bail!(
                "{}: nenhuma call a Controller, Service ou Component encontrada",
                self.path.display()
            );
        };

        let mut methods = Vec::new();
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        let mut deps: BTreeSet<String> = BTreeSet::new();

        for st in self.root.clone() {
            let Some((name, func)) = Self::method_body(&st, &local_name) else {
                continue;
            };
            let signature = self.signature(&func, true);
            methods.push(Method { name, signature });
            self.scan_body(&func, &mut fields, &mut deps);
        }

        Ok(Module {
            id,
            kind,
            file: self.path.to_string_lossy().replace('\\', "/"),
            services: self.services(),
            requires: self.requires(),
            local_types: self.local_types(),
            methods,
            fields: fields
                .into_iter()
                .map(|(name, ty)| Field { name, ty })
                .collect(),
            dependencies: deps.into_iter().collect(),
            declared_require,
            issues: self.issues,
        })
    }

    fn declaration(&self) -> Option<(String, String, Vec<String>, String)> {
        for st in &self.root {
            if kind_of(st) != "AstStatLocal" {
                continue;
            }
            let Some(call) = list(st, "values").first() else {
                continue;
            };
            if kind_of(call) != "AstExprCall" {
                continue;
            }
            let func = call.get("func")?;
            if kind_of(func) != "AstExprIndexName" {
                continue;
            }
            let kind = text(func, "index");
            if !matches!(kind, "Controller" | "Service" | "Component") {
                continue;
            }
            let args = list(call, "args");
            let first_arg = args.first()?;
            if kind_of(first_arg) != "AstExprConstantString" {
                continue;
            }
            let local_name = list(st, "vars").first().map(|v| text(v, "name"))?.to_string();
            return Some((
                local_name,
                text(first_arg, "value").to_string(),
                Self::require_from_props(args),
                kind.to_string(),
            ));
        }
        None
    }

    fn require_from_props(args: &[Value]) -> Vec<String> {
        let Some(props) = args.get(1).filter(|p| kind_of(p) == "AstExprTable") else {
            return Vec::new();
        };
        for item in list(props, "items") {
            let key = item.get("key").map(|k| text(k, "value")).unwrap_or("");
            if key != "Require" {
                continue;
            }
            let Some(v) = item.get("value") else { continue };
            if kind_of(v) == "AstExprConstantString" {
                return vec![text(v, "value").to_string()];
            }
            if kind_of(v) == "AstExprTable" {
                return list(v, "items")
                    .iter()
                    .filter_map(|i| i.get("value"))
                    .filter(|val| kind_of(val) == "AstExprConstantString")
                    .map(|val| text(val, "value").to_string())
                    .collect();
            }
        }
        Vec::new()
    }

    fn method_body(st: &Value, target: &str) -> Option<(String, Value)> {
        if kind_of(st) == "AstStatFunction" {
            let name = st.get("name")?;
            if kind_of(name) == "AstExprIndexName" && is_local(name.get("expr")?, target) {
                return Some((text(name, "index").to_string(), st.get("func")?.clone()));
            }
        }
        if kind_of(st) == "AstStatAssign" {
            let var = list(st, "vars").first()?;
            let val = list(st, "values").first()?;
            if kind_of(var) == "AstExprIndexName"
                && is_local(var.get("expr")?, target)
                && kind_of(val) == "AstExprFunction"
            {
                return Some((text(var, "index").to_string(), val.clone()));
            }
        }
        None
    }

    fn scan_body(
        &mut self,
        func: &Value,
        fields: &mut BTreeMap<String, String>,
        deps: &mut BTreeSet<String>,
    ) {
        let mut atribuicoes: Vec<(String, Value)> = Vec::new();
        if let Some(body) = func.get("body") {
            walk(body, &mut |node: &Value| {
                if kind_of(node) == "AstStatAssign" {
                    let vars = list(node, "vars");
                    let vals = list(node, "values");
                    for (var, val) in vars.iter().zip(vals.iter()) {
                        if kind_of(var) != "AstExprIndexName" {
                            continue;
                        }
                        let Some(inside) = var.get("expr") else { continue };
                        if !is_local(inside, "self") {
                            continue;
                        }
                        atribuicoes.push((text(var, "index").to_string(), val.clone()));
                    }
                }
                if kind_of(node) == "AstExprIndexName" {
                    if let Some(inside) = node.get("expr") {
                        if indexes(inside, "Dependencies")
                            && inside.get("expr").is_some_and(|e| is_local(e, "self"))
                        {
                            deps.insert(text(node, "index").to_string());
                        }
                    }
                }
            });
        }

        for (name, value) in atribuicoes {
            if fields.contains_key(&name) {
                continue;
            }
            if let Some(t) = self.value_type(&value, &format!("self.{name}")) {
                fields.insert(name, t);
            }
        }
    }

    fn services(&self) -> Vec<Service> {
        let mut out = Vec::new();
        for st in &self.root {
            if kind_of(st) != "AstStatLocal" {
                continue;
            }
            let Some(v) = list(st, "values").first() else { continue };
            if kind_of(v) != "AstExprCall" {
                continue;
            }
            let Some(func) = v.get("func") else { continue };
            if kind_of(func) != "AstExprIndexName" || text(func, "index") != "GetService" {
                continue;
            }
            let Some(arg) = list(v, "args").first() else { continue };
            if kind_of(arg) != "AstExprConstantString" {
                continue;
            }
            let Some(alias) = list(st, "vars").first().map(|x| text(x, "name")) else {
                continue;
            };
            out.push(Service {
                alias: alias.to_string(),
                service: text(arg, "value").to_string(),
            });
        }
        out
    }

    fn requires(&self) -> Vec<Import> {
        let mut out = Vec::new();
        for st in &self.root {
            if kind_of(st) != "AstStatLocal" {
                continue;
            }
            let Some(v) = list(st, "values").first() else { continue };
            if kind_of(v) != "AstExprCall" {
                continue;
            }
            let Some(func) = v.get("func") else { continue };
            let eh_require = (kind_of(func) == "AstExprGlobal" && text(func, "global") == "require")
                || (kind_of(func) == "AstExprLocal"
                    && func.get("local").map(|l| text(l, "name")) == Some("require"));
            if !eh_require {
                continue;
            }
            let Some(arg) = list(v, "args").first() else { continue };
            let Some(alias) = list(st, "vars").first().map(|x| text(x, "name")) else {
                continue;
            };
            out.push(Import {
                alias: alias.to_string(),
                expr: self.fonte.slice(text(arg, "location")).trim().to_string(),
            });
        }
        out
    }

    fn local_types(&self) -> Vec<LocalType> {
        self.root
            .iter()
            .filter(|st| kind_of(st) == "AstStatTypeAlias")
            .map(|st| LocalType {
                name: text(st, "name").to_string(),
                text: self.fonte.slice(text(st, "location")).trim().to_string(),
            })
            .collect()
    }
}
