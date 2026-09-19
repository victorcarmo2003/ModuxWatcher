use std::fmt::Display;
use std::path::Path;

use anyhow::{Context, Result};
use full_moon::ast::Ast;
use full_moon::node::Node;

pub fn parse(path: &Path) -> Result<Ast> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;

    full_moon::parse_fallible(&text, full_moon::LuaVersion::luau())
        .into_result()
        .map_err(|errors| {
            let shown: Vec<String> = errors.iter().take(3).map(ToString::to_string).collect();
            anyhow::anyhow!("{}:\n  {}", path.display(), shown.join("\n  "))
        })
}

// full_moon renders a node completely through Display, but the text carries the
// first token's leading trivia and the last token's trailing trivia, so a
// statement preceded by a comment comes back with the comment glued on.
//
// Slicing the source by Node positions looks like the obvious fix and is wrong:
// for a table type the closing brace is outside both `end_position()` and
// `tokens()`, so `{ Name: string }` comes back as `{ Name: string`. Cutting the
// trivia off the rendered text is the only form that survives both.
pub fn span<T: Node + Display>(node: &T) -> String {
    let full = node.to_string();
    let tokens: Vec<_> = node.tokens().collect();

    let joined = |trivia: &mut dyn Iterator<Item = String>| -> String { trivia.collect() };

    let lead = tokens
        .first()
        .map(|t| joined(&mut t.leading_trivia().map(|x| x.to_string())))
        .unwrap_or_default();
    let trail = tokens
        .last()
        .map(|t| joined(&mut t.trailing_trivia().map(|x| x.to_string())))
        .unwrap_or_default();

    // Cortar por comprimento seria errado sempre que o render terminar em algo
    // que nao e o ultimo token: um table type fecha com `}` que nao aparece em
    // `tokens()`, entao descontar o tamanho da trivia comia a chave e
    // `{ string }` virava `{ string`, que nao e Luau. Descontar so quando a
    // trivia esta mesmo na borda cobre os dois casos.
    let start = if !lead.is_empty() && full.starts_with(&lead) { lead.len() } else { 0 };
    let end = if !trail.is_empty() && full.ends_with(&trail) {
        full.len() - trail.len()
    } else {
        full.len()
    };

    full[start..end.max(start)].trim().to_string()
}

