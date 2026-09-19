use std::fmt::Display;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use full_moon::ast::Ast;
use full_moon::node::Node;

/// Um arquivo a meio caminho de ser escrito nao e uma falha do projeto, e o
/// watch precisa saber a diferenca: la isso acontece a cada tecla, e o dump do
/// full_moon, com tres erros de duas linhas cada, enterra todo o resto do log.
/// Fora do watch a mensagem inteira e o que interessa, entao ela viaja junto.
#[derive(Debug)]
pub struct Syntax {
    pub file: PathBuf,
    pub line: Option<usize>,
    pub detail: String,
}

impl Display for Syntax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:\n  {}", self.file.display(), self.detail)
    }
}

impl std::error::Error for Syntax {}

fn first_line(message: &str) -> Option<usize> {
    let at = message.find("line ")? + "line ".len();
    message[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// Devolve a arvore e, quando o arquivo nao fecha, o que faltou.
///
/// `parse_fallible` reconstroi uma arvore mesmo com erro, e e isso que salva a
/// digitacao: quem escreve `self.` numa funcao la embaixo nao pode perder o
/// tipo das funcoes que ja estavam prontas acima. Descartar a arvore inteira
/// por causa de um trecho incompleto era o que congelava a tipagem do arquivo
/// ate a ultima tecla.
///
/// Quem chama decide o que fazer com o resto: se a declaracao do modulo
/// sobreviveu a reconstrucao, da para gerar; se nao, `Extractor::run` reclama
/// por conta propria.
pub fn parse(path: &Path) -> Result<(Ast, Option<Syntax>)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;

    let parsed = full_moon::parse_fallible(&text, full_moon::LuaVersion::luau());
    let shown: Vec<String> = parsed.errors().iter().take(3).map(ToString::to_string).collect();

    let broken = shown.first().map(|first| Syntax {
        line: first_line(first),
        file: path.to_path_buf(),
        detail: shown.join("\n  "),
    });

    Ok((parsed.into_ast(), broken))
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

