
use std::path::{Path, PathBuf};

use crate::extract::Module;

/// A type annotation is transcribed from the source verbatim, so a multi-line
/// one arrives carrying whatever indentation it had where it was written. An
/// annotation nested inside an `if` inside a `while` would land four tabs deep
/// in a leaf that only has one level. Valid Luau either way, but the leaf is a
/// file people read on hover, so strip the original indentation and lay it out
/// against `at`.
fn reindent(text: &str, at: &str) -> String {
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return text.to_string();
    };
    let rest: Vec<&str> = lines.collect();
    if rest.is_empty() {
        return first.to_string();
    }

    let common = rest
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    let mut out = String::from(first);
    for line in rest {
        out.push('\n');
        if line.trim().is_empty() {
            continue;
        }
        out.push_str(at);
        out.push_str(&line[common.min(line.len())..]);
    }
    out
}

pub fn rewrite_require(expr: &str) -> String {
    if expr == "script" || expr.starts_with("script.") {
        format!("script.Parent{}", &expr["script".len()..])
    } else {
        expr.to_string()
    }
}

fn cites(texts: &[String], name: &str) -> bool {
    texts.iter().any(|t| has_word(t, name))
}

fn has_word(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let target = name.as_bytes();
    let is_word_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';

    let mut i = 0;
    while let Some(pos) = text[i..].find(name) {
        let start_at = i + pos;
        let end_at = start_at + target.len();
        let before_ok = start_at == 0 || !is_word_byte(bytes[start_at - 1]);
        let after_ok = end_at >= bytes.len() || !is_word_byte(bytes[end_at]);
        if before_ok && after_ok {
            return true;
        }
        i = start_at + 1;
        if i >= text.len() {
            break;
        }
    }
    false
}

/// A require is copied into the leaf when the annotation reaches through it,
/// which is almost always `Alias.Something`. The exception is `typeof(Alias)`,
/// where the module is named on its own: nothing follows it but the closing
/// paren, so matching on a trailing dot misses it and the leaf ends up with
/// `typeof(Template)` and no Template. That reads as an unknown global, the
/// exported type becomes an error type, and because the Manifest indexes every
/// leaf, one such line takes down the typing of every module on that side.
fn cites_alias(texts: &[String], alias: &str) -> bool {
    texts.iter().any(|t| {
        let mut i = 0;
        while let Some(pos) = t[i..].find(alias) {
            let start_at = i + pos;
            let end_at = start_at + alias.len();
            let before_ok = start_at == 0 || !t.as_bytes()[start_at - 1].is_ascii_alphanumeric();
            let after = t[end_at..].trim_start();
            let wrapped = after.starts_with(')') && t[..start_at].trim_end().ends_with("typeof(");
            if before_ok && (after.starts_with('.') || wrapped) {
                return true;
            }
            i = start_at + 1;
            if i >= t.len() {
                break;
            }
        }
        false
    })
}

pub fn leaf_path(origin: &Path) -> PathBuf {
    origin.parent().unwrap_or(Path::new(".")).join("Type.luau")
}

pub fn emit(m: &Module) -> String {
    let mut members: Vec<(String, String)> = Vec::new();

    // A component object always has the Instance it was built for, so `Public`
    // is the wrong shape without it. This used to typecheck by accident: the
    // declaring file sees `self.Instance` because `SelfOf.Build` grafts it on,
    // and a method body is not rechecked at the call site, so nobody noticed
    // that the type the Manifest hands out had no Instance on it.
    if m.kind == "Component" && !m.fields.iter().any(|f| f.name == "Instance") {
        members.push(("Instance".to_string(), "Instance".to_string()));
    }

    members.extend(m.fields.iter().map(|c| (c.name.clone(), c.ty.clone())));
    members.extend(m.methods.iter().map(|x| (x.name.clone(), x.signature.clone())));
    let texts: Vec<String> = members.iter().map(|(_, t)| t.clone()).collect();

    let mut locals = Vec::new();
    let mut stale: Vec<_> = m.local_types.iter().collect();
    let mut target = texts.clone();
    loop {
        let before = locals.len();
        let mut rest = Vec::new();
        for tl in stale {
            if cites(&target, &tl.name) {
                target.push(tl.text.clone());
                locals.push(tl);
            } else {
                rest.push(tl);
            }
        }
        stale = rest;
        if locals.len() == before {
            break;
        }
    }

    // After the local types, not before: a local type is copied into the leaf
    // whole, so a require that only the local type reaches through still has to
    // come along. `target` is the members plus every local type that was taken.
    let requires: Vec<_> = m.requires.iter().filter(|r| cites_alias(&target, &r.alias)).collect();

    let exprs: Vec<String> = requires.iter().map(|r| rewrite_require(&r.expr)).collect();
    let services: Vec<_> = m
        .services
        .iter()
        .filter(|s| exprs.iter().any(|e| e.starts_with(&format!("{}.", s.alias))))
        .collect();

    let mut blocks = vec!["--!strict\n".to_string()];

    let mut header = Vec::new();
    for s in &services {
        header.push(format!(
            "local {} = game:GetService(\"{}\")",
            s.alias, s.service
        ));
    }
    for (i, r) in requires.iter().enumerate() {
        header.push(format!("local {} = require({})", r.alias, exprs[i]));
    }
    if !header.is_empty() {
        blocks.push(format!("{}\n", header.join("\n")));
    }

    if !locals.is_empty() {
        let copied: Vec<&str> = locals.iter().map(|t| t.text.as_str()).collect();
        blocks.push(format!("{}\n", copied.join("\n")));
    }

    let mut body = vec!["export type Public = {".to_string()];
    for (name, ty) in &members {
        body.push(format!("\t{name}: {},", reindent(ty, "\t")));
    }
    body.push("}".to_string());
    blocks.push(format!("{}\n", body.join("\n")));

    blocks.push("return {}\n".to_string());
    blocks.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Vec<String> {
        vec![v.to_string()]
    }

    #[test]
    fn alias_through_a_dot() {
        assert!(cites_alias(&s("Signal.Signal<Player>"), "Signal"));
        assert!(cites_alias(&s("{ a: Lib.Thing, b: number }"), "Lib"));
    }

    #[test]
    fn alias_alone_inside_typeof() {
        assert!(cites_alias(&s("typeof(Template)"), "Template"));
        assert!(cites_alias(&s("{ data: typeof(Template)? }"), "Template"));
    }

    #[test]
    fn a_bare_mention_is_not_a_citation() {
        assert!(!cites_alias(&s("(Template) -> ()"), "Template"));
        assert!(!cites_alias(&s("Templated.Thing"), "Template"));
    }
}
