use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use full_moon::ast::{
    Assignment, Ast, Call, Expression, FunctionArgs, FunctionBody, FunctionCall, Index, Parameter,
    Prefix, Stmt, Suffix, Var, VarExpression,
};
use full_moon::node::Node;
use full_moon::tokenizer::TokenReference;
use full_moon::visitors::{Visit, Visitor};
use serde::Serialize;

use crate::ast::{self, span};

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

fn name_of(token: &TokenReference) -> String {
    token.token().to_string()
}

fn string_value(expr: &Expression) -> Option<String> {
    let Expression::String(token) = expr else {
        return None;
    };
    let raw = token.token().to_string();
    let opener = raw.chars().next()?;
    let quoted = raw.len() >= 2 && (opener == '"' || opener == '\'') && raw.ends_with(opener);
    Some(if quoted {
        raw[1..raw.len() - 1].to_string()
    } else {
        raw
    })
}

/// Os parametros do metodo, com o tipo que a pessoa ja declarou na assinatura.
/// Serve para `self.Campo = parametro`, que e a forma mais comum de encher o
/// self e antes era descartada: o gerador so lia literal, e pedia um `::`
/// redundante logo ao lado de uma anotacao que ja existia dois palmos acima.
fn typed_parameters(body: &FunctionBody) -> BTreeMap<String, String> {
    let params: Vec<&Parameter> = body.parameters().iter().collect();
    let types: Vec<_> = body.type_specifiers().collect();

    let mut out = BTreeMap::new();
    for (i, p) in params.iter().enumerate() {
        let Parameter::Name(_) = p else { continue };
        if let Some(ts) = types.get(i).copied().flatten() {
            out.insert(param_name(p), span(ts.type_info()));
        }
    }
    out
}

fn param_name(p: &Parameter) -> String {
    match p {
        Parameter::Name(t) => name_of(t),
        _ => "...".to_string(),
    }
}

/// One link of a call or index chain, flattened so that `a.b:c(x)` reads as
/// [Dot("b"), Method("c", args)] regardless of which node type it came from.
enum Step<'a> {
    Dot(String),
    Method(String, &'a FunctionArgs),
    Call(&'a FunctionArgs),
    Other,
}

fn steps<'a>(suffixes: impl Iterator<Item = &'a Suffix>) -> Vec<Step<'a>> {
    suffixes
        .map(|s| match s {
            Suffix::Index(Index::Dot { name, .. }) => Step::Dot(name_of(name)),
            Suffix::Call(Call::MethodCall(m)) => Step::Method(name_of(m.name()), m.args()),
            Suffix::Call(Call::AnonymousCall(args)) => Step::Call(args),
            _ => Step::Other,
        })
        .collect()
}

fn prefix_name(prefix: &Prefix) -> Option<String> {
    match prefix {
        Prefix::Name(t) => Some(name_of(t)),
        _ => None,
    }
}

fn arguments(args: &FunctionArgs) -> Vec<&Expression> {
    match args {
        FunctionArgs::Parentheses { arguments, .. } => arguments.iter().collect(),
        _ => Vec::new(),
    }
}

/// `local x = ...` and `const x = ...` bind the same way; the extractor does not
/// care which keyword was used.
fn binding(st: &Stmt) -> Option<(Vec<String>, Vec<&Expression>)> {
    match st {
        Stmt::LocalAssignment(l) => Some((
            l.names().iter().map(name_of).collect(),
            l.expressions().iter().collect(),
        )),
        Stmt::ConstAssignment(c) => Some((
            c.names().iter().map(name_of).collect(),
            c.expressions().iter().collect(),
        )),
        _ => None,
    }
}

/// `self.Dependencies.X`, in any position, whether or not it ends in a call.
fn dependency_read(prefix: &Prefix, chain: &[Step]) -> Option<String> {
    if prefix_name(prefix).as_deref() != Some("self") {
        return None;
    }
    match (chain.first(), chain.get(1)) {
        (Some(Step::Dot(outer)), Some(Step::Dot(inner))) if outer == "Dependencies" => {
            Some(inner.clone())
        }
        _ => None,
    }
}

/// `self.X`, exactly one level deep.
fn self_field(var: &VarExpression) -> Option<String> {
    if prefix_name(var.prefix()).as_deref() != Some("self") {
        return None;
    }
    match steps(var.suffixes()).as_slice() {
        [Step::Dot(name)] => Some(name.clone()),
        _ => None,
    }
}

#[derive(Default)]
struct BodyScan {
    assignments: Vec<(String, Expression)>,
    deps: BTreeSet<String>,
}

impl Visitor for BodyScan {
    fn visit_assignment(&mut self, node: &Assignment) {
        for (var, value) in node.variables().iter().zip(node.expressions().iter()) {
            if let Var::Expression(ve) = var {
                if let Some(name) = self_field(ve) {
                    self.assignments.push((name, value.clone()));
                }
            }
        }
    }

    fn visit_var_expression(&mut self, node: &VarExpression) {
        if let Some(d) = dependency_read(node.prefix(), &steps(node.suffixes())) {
            self.deps.insert(d);
        }
    }

    fn visit_function_call(&mut self, node: &FunctionCall) {
        if let Some(d) = dependency_read(node.prefix(), &steps(node.suffixes())) {
            self.deps.insert(d);
        }
    }
}

pub struct Extractor {
    path: PathBuf,
    ast: Ast,
    broken: Option<crate::ast::Syntax>,
    issues: Vec<Issue>,
}

impl Extractor {
    pub fn new(path: &Path) -> Result<Self> {
        let (ast, broken) = ast::parse(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            ast,
            broken,
            issues: Vec::new(),
        })
    }

    fn issue<T: Node>(&mut self, node: &T, message: String) {
        let at = node.start_position();
        self.issues.push(Issue {
            line: at.map(|p| p.line()).unwrap_or(0),
            column: at.map(|p| p.character()).unwrap_or(0),
            message,
        });
    }

    fn value_type(
        &mut self,
        value: &Expression,
        where_at: &str,
        known: &BTreeMap<String, String>,
    ) -> Option<String> {
        if let Expression::Var(Var::Name(name)) = value {
            if let Some(ty) = known.get(&name_of(name)) {
                return Some(ty.clone());
            }
        }
        match value {
            Expression::TypeAssertion { type_assertion, .. } => {
                return Some(span(type_assertion.cast_to()))
            }
            Expression::String(_) => return Some("string".to_string()),
            Expression::Number(_) => return Some("number".to_string()),
            Expression::Symbol(t) => {
                let s = name_of(t);
                if s == "true" || s == "false" {
                    return Some("boolean".to_string());
                }
            }
            Expression::Function(f) => return Some(self.signature(f.body(), false, false)),
            _ => {}
        }
        self.issue(
            value,
            format!(
                "{where_at} has no type. Annotate it with `::` so the generator can transcribe it."
            ),
        );
        None
    }

    /// `with_self` prepends the receiver. `colon` says the declaration used
    /// method syntax, where `self` is implicit and there is no first parameter
    /// to drop; with dot syntax an explicit `self` parameter is that same
    /// receiver spelled out, so it goes.
    fn signature(&mut self, body: &FunctionBody, colon: bool, with_self: bool) -> String {
        let params: Vec<&Parameter> = body.parameters().iter().collect();
        let types: Vec<_> = body.type_specifiers().collect();

        let mut parts = Vec::new();
        let mut first = 0;
        if with_self {
            parts.push("self: Public".to_string());
            if !colon && params.first().map(|p| param_name(p)).as_deref() == Some("self") {
                first = 1;
            }
        }

        for i in first..params.len() {
            let name = param_name(params[i]);
            match types.get(i).copied().flatten() {
                Some(ts) => parts.push(format!("{name}: {}", span(ts.type_info()))),
                None => {
                    self.issue(body, format!("parameter `{name}` has no type annotation."));
                    parts.push(format!("{name}: unknown"));
                }
            }
        }

        let returns = match body.return_type() {
            Some(ts) => span(ts.type_info()),
            None => "()".to_string(),
        };
        format!("({}) -> {returns}", parts.join(", "))
    }

    pub fn run(mut self) -> Result<Module> {
        // O arquivo nao fechou, mas a arvore reconstruida pode ter conservado o
        // suficiente. Vira um aviso na lista, nao uma recusa: o que sobrou
        // ainda serve, e o leaf de agora e melhor que o leaf de tres teclas
        // atras.
        if let Some(broken) = self.broken.take() {
            self.issues.push(Issue {
                line: broken.line.unwrap_or(0),
                column: 0,
                message: "file does not parse yet; typed from what was readable".to_string(),
            });
        }

        let Some((local_name, id, declared_require, kind)) = self.declaration() else {
            bail!(
                "{}: no call to Controller, Service or Component found",
                self.path.display()
            );
        };

        let statements: Vec<Stmt> = self.ast.nodes().stmts().cloned().collect();

        let mut methods = Vec::new();
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        let mut deps: BTreeSet<String> = BTreeSet::new();

        for st in &statements {
            let Some((name, body, colon)) = Self::method_body(st, &local_name) else {
                continue;
            };
            let signature = self.signature(&body, colon, true);
            methods.push(Method { name, signature });

            let params = typed_parameters(&body);
            let mut scan = BodyScan::default();
            body.block().visit(&mut scan);
            deps.extend(scan.deps);
            for (field, value) in scan.assignments {
                if fields.contains_key(&field) {
                    continue;
                }
                if let Some(ty) = self.value_type(&value, &format!("self.{field}"), &params) {
                    fields.insert(field, ty);
                }
            }
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
        for st in self.ast.nodes().stmts() {
            let Some((names, values)) = binding(st) else {
                continue;
            };
            let Some(Expression::FunctionCall(call)) = values.first() else {
                continue;
            };
            let chain = steps(call.suffixes());
            let [Step::Dot(kind), Step::Call(args)] = chain.as_slice() else {
                continue;
            };
            if !matches!(kind.as_str(), "Controller" | "Service" | "Component") {
                continue;
            }
            let args = arguments(args);
            let id = args.first().and_then(|a| string_value(a))?;
            let local_name = names.first()?.clone();
            return Some((local_name, id, Self::require_from_props(&args), kind.clone()));
        }
        None
    }

    fn require_from_props(args: &[&Expression]) -> Vec<String> {
        let Some(Expression::TableConstructor(props)) = args.get(1) else {
            return Vec::new();
        };
        for field in props.fields() {
            let full_moon::ast::Field::NameKey { key, value, .. } = field else {
                continue;
            };
            if name_of(key) != "Require" {
                continue;
            }
            if let Some(one) = string_value(value) {
                return vec![one];
            }
            if let Expression::TableConstructor(list) = value {
                return list
                    .fields()
                    .iter()
                    .filter_map(|f| match f {
                        full_moon::ast::Field::NoKey(v) => string_value(v),
                        _ => None,
                    })
                    .collect();
            }
        }
        Vec::new()
    }

    fn method_body(st: &Stmt, target: &str) -> Option<(String, FunctionBody, bool)> {
        if let Stmt::FunctionDeclaration(f) = st {
            let names: Vec<String> = f.name().names().iter().map(name_of).collect();
            if let Some(method) = f.name().method_name() {
                if names.as_slice() == [target] {
                    return Some((name_of(method), f.body().clone(), true));
                }
            } else if names.len() == 2 && names[0] == target {
                return Some((names[1].clone(), f.body().clone(), false));
            }
        }

        if let Stmt::Assignment(a) = st {
            let var = a.variables().iter().next()?;
            let value = a.expressions().iter().next()?;
            let Var::Expression(ve) = var else {
                return None;
            };
            if prefix_name(ve.prefix()).as_deref() != Some(target) {
                return None;
            }
            let chain = steps(ve.suffixes());
            let [Step::Dot(name)] = chain.as_slice() else {
                return None;
            };
            if let Expression::Function(f) = value {
                return Some((name.clone(), f.body().clone(), false));
            }
        }

        None
    }

    fn services(&self) -> Vec<Service> {
        let mut out = Vec::new();
        for st in self.ast.nodes().stmts() {
            let Some((names, values)) = binding(st) else {
                continue;
            };
            let Some(Expression::FunctionCall(call)) = values.first() else {
                continue;
            };
            let chain = steps(call.suffixes());
            let args = match chain.as_slice() {
                [Step::Method(name, args)] if name == "GetService" => *args,
                [Step::Dot(name), Step::Call(args)] if name == "GetService" => *args,
                _ => continue,
            };
            let Some(service) = arguments(args).first().and_then(|a| string_value(a)) else {
                continue;
            };
            let Some(alias) = names.first() else {
                continue;
            };
            out.push(Service {
                alias: alias.clone(),
                service,
            });
        }
        out
    }

    fn requires(&self) -> Vec<Import> {
        let mut out = Vec::new();
        for st in self.ast.nodes().stmts() {
            let Some((names, values)) = binding(st) else {
                continue;
            };
            let Some(Expression::FunctionCall(call)) = values.first() else {
                continue;
            };
            if prefix_name(call.prefix()).as_deref() != Some("require") {
                continue;
            }
            let chain = steps(call.suffixes());
            let [Step::Call(args)] = chain.as_slice() else {
                continue;
            };
            let Some(arg) = arguments(args).first().copied() else {
                continue;
            };
            let Some(alias) = names.first() else {
                continue;
            };
            out.push(Import {
                alias: alias.clone(),
                expr: span(arg),
            });
        }
        out
    }

    fn local_types(&self) -> Vec<LocalType> {
        let mut out = Vec::new();
        for st in self.ast.nodes().stmts() {
            let name = match st {
                Stmt::TypeDeclaration(d) => name_of(d.type_name()),
                Stmt::ExportedTypeDeclaration(e) => name_of(e.type_declaration().type_name()),
                _ => continue,
            };
            out.push(LocalType {
                name,
                text: span(st),
            });
        }
        out
    }
}
