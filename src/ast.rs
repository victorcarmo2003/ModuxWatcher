//! Roda o `luau-ast` e da acesso ao JSON dele.
//!
//! A arvore nao e desserializada em structs: os nos tem forma variavel demais e
//! so uma fracao dos campos interessa. `serde_json::Value` com acessores curtos
//! sai mais barato de manter do que acompanhar a gramatica inteira.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// Nome do binario procurado no PATH. O Rokit instala ele junto quando o
/// projeto declara `luau-lang/luau` no rokit.toml.
pub const BIN: &str = "luau-ast";

/// Texto do arquivo, fatiavel por `location`.
///
/// Os tipos sao recuperados fatiando o FONTE ORIGINAL em vez de reimprimir a
/// partir da arvore. Assim `Template.Data`, `FSM.FSM<Estado>` e
/// `(boolean, string)` saem exatamente como foram digitados, e nao existe um
/// impressor de tipos para manter em dia com a gramatica.
pub struct Fonte {
    linhas: Vec<String>,
}

impl Fonte {
    pub fn nova(texto: &str) -> Self {
        Self {
            linhas: texto.replace("\r\n", "\n").split('\n').map(str::to_string).collect(),
        }
    }

    fn ponto(s: &str) -> (usize, usize) {
        let mut partes = s.trim().split(',');
        let linha = partes.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let coluna = partes.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        (linha, coluna)
    }

    /// `location` vem como `"6,27 - 6,31"`, com linha e coluna base zero.
    pub fn fatia(&self, location: &str) -> String {
        let mut lados = location.split('-');
        let (l0, c0) = Self::ponto(lados.next().unwrap_or("0,0"));
        let (l1, c1) = Self::ponto(lados.next().unwrap_or("0,0"));

        let pegar = |i: usize| self.linhas.get(i).map(String::as_str).unwrap_or("");
        let corte = |s: &str, ini: usize, fim: usize| -> String {
            let chars: Vec<char> = s.chars().collect();
            let ini = ini.min(chars.len());
            let fim = fim.min(chars.len());
            chars[ini..fim].iter().collect()
        };

        if l0 == l1 {
            return corte(pegar(l0), c0, c1);
        }
        let mut partes = vec![corte(pegar(l0), c0, usize::MAX)];
        for i in (l0 + 1)..l1 {
            partes.push(pegar(i).to_string());
        }
        partes.push(corte(pegar(l1), 0, c1));
        partes.join("\n")
    }
}

/// Executa o `luau-ast` e devolve a raiz (`root.body`) junto com o fonte.
pub fn analisar(caminho: &Path) -> Result<(Vec<Value>, Fonte)> {
    let texto = std::fs::read_to_string(caminho)
        .with_context(|| format!("nao consegui ler {}", caminho.display()))?;

    let saida = Command::new(BIN).arg(caminho).output().with_context(|| {
        format!(
            "nao encontrei `{BIN}` no PATH.\n\
             Instale com:  rokit add luau-lang/luau\n\
             (o mesmo pacote traz o luau-analyze, que voce vai querer de qualquer jeito)"
        )
    })?;

    if !saida.status.success() {
        bail!(
            "{BIN} falhou em {}:\n{}",
            caminho.display(),
            String::from_utf8_lossy(&saida.stderr).trim()
        );
    }

    let json: Value = serde_json::from_slice(&saida.stdout)
        .with_context(|| format!("saida do {BIN} nao e JSON valido para {}", caminho.display()))?;

    let corpo = json
        .get("root")
        .and_then(|r| r.get("body"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    Ok((corpo, Fonte::nova(&texto)))
}

// ---- acessores curtos, para o resto do codigo nao virar sopa de `.get()` ----

pub fn tipo(no: &Value) -> &str {
    no.get("type").and_then(Value::as_str).unwrap_or("")
}

pub fn texto<'a>(no: &'a Value, chave: &str) -> &'a str {
    no.get(chave).and_then(Value::as_str).unwrap_or("")
}

pub fn lista<'a>(no: &'a Value, chave: &str) -> &'a [Value] {
    no.get(chave).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// Verdadeiro se o no for `AstExprLocal` apontando para o local `nome`.
pub fn eh_local(no: &Value, nome: &str) -> bool {
    tipo(no) == "AstExprLocal" && no.get("local").map(|l| texto(l, "name")) == Some(nome)
}

/// Verdadeiro se o no for `<algo>.<nome>`.
pub fn indexa(no: &Value, nome: &str) -> bool {
    tipo(no) == "AstExprIndexName" && texto(no, "index") == nome
}

/// Percorre a arvore inteira chamando `visita` em cada no que tenha `type`.
pub fn caminha(no: &Value, visita: &mut dyn FnMut(&Value)) {
    match no {
        Value::Object(mapa) => {
            if mapa.contains_key("type") {
                visita(no);
            }
            for v in mapa.values() {
                caminha(v, visita);
            }
        }
        Value::Array(itens) => {
            for v in itens {
                caminha(v, visita);
            }
        }
        _ => {}
    }
}
