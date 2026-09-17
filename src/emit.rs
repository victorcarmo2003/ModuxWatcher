//! Escreve a folha de tipos (`Type.luau`) a partir do que o extrator achou.
//!
//! Transcricao pura: tudo aqui ja veio pronto do corpo. O emissor so decide o
//! que INCLUIR e como reescrever os caminhos de require.

use std::path::{Path, PathBuf};

use crate::extract::Modulo;

/// Caminho relativo sobe um nivel ao sair do `init.luau` para o `Type.luau`.
///
/// No corpo o `script` E a pasta do modulo; na folha o `script` e o proprio
/// arquivo e `script.Parent` e a pasta. Sem reescrever, `script.Template`
/// apontaria um nivel acima — e o Rojo monta sem reclamar, entao o erro so
/// aparece em runtime.
pub fn reescreve_require(expressao: &str) -> String {
    if expressao == "script" || expressao.starts_with("script.") {
        format!("script.Parent{}", &expressao["script".len()..])
    } else {
        expressao.to_string()
    }
}

/// O identificador aparece como palavra inteira em algum dos textos?
///
/// Comparacao manual em vez de regex: e a unica coisa que pediria a dependencia,
/// e as bordas de palavra aqui sao simples (alfanumerico ou `_`).
fn cita(textos: &[String], nome: &str) -> bool {
    textos.iter().any(|t| contem_palavra(t, nome))
}

fn contem_palavra(texto: &str, nome: &str) -> bool {
    if nome.is_empty() {
        return false;
    }
    let bytes = texto.as_bytes();
    let alvo = nome.as_bytes();
    let parte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';

    let mut i = 0;
    while let Some(pos) = texto[i..].find(nome) {
        let ini = i + pos;
        let fim = ini + alvo.len();
        let antes_ok = ini == 0 || !parte(bytes[ini - 1]);
        let depois_ok = fim >= bytes.len() || !parte(bytes[fim]);
        if antes_ok && depois_ok {
            return true;
        }
        i = ini + 1;
        if i >= texto.len() {
            break;
        }
    }
    false
}

/// O alias aparece como prefixo de tipo (`Alias.Algo`)?
fn cita_alias(textos: &[String], alias: &str) -> bool {
    textos.iter().any(|t| {
        let mut i = 0;
        while let Some(pos) = t[i..].find(alias) {
            let ini = i + pos;
            let fim = ini + alias.len();
            let antes_ok = ini == 0 || !t.as_bytes()[ini - 1].is_ascii_alphanumeric();
            let depois = t[fim..].trim_start();
            if antes_ok && depois.starts_with('.') {
                return true;
            }
            i = ini + 1;
            if i >= t.len() {
                break;
            }
        }
        false
    })
}

pub fn caminho_da_folha(origem: &Path) -> PathBuf {
    origem.parent().unwrap_or(Path::new(".")).join("Type.luau")
}

pub fn emitir(m: &Modulo) -> String {
    let mut membros: Vec<(String, String)> = m
        .campos
        .iter()
        .map(|c| (c.nome.clone(), c.tipo.clone()))
        .collect();
    membros.extend(m.metodos.iter().map(|x| (x.nome.clone(), x.assinatura.clone())));
    let textos: Vec<String> = membros.iter().map(|(_, t)| t.clone()).collect();

    // So entra o require que algum tipo emitido realmente usa. Isso exclui o
    // `Classes` sem regra especial — e precisa excluir: se ele entrasse, a folha
    // requereria o modulo que requer o Manifest que requer a folha.
    let requires: Vec<_> = m.requires.iter().filter(|r| cita_alias(&textos, &r.alias)).collect();

    // Tipo local so entra se algum membro citar, e ele pode citar outro tipo
    // local, entao a varredura repete ate estabilizar.
    let mut locais = Vec::new();
    let mut pendentes: Vec<_> = m.tipos_locais.iter().collect();
    let mut alvo = textos.clone();
    loop {
        let antes = locais.len();
        let mut restantes = Vec::new();
        for tl in pendentes {
            if cita(&alvo, &tl.nome) {
                alvo.push(tl.texto.clone());
                locais.push(tl);
            } else {
                restantes.push(tl);
            }
        }
        pendentes = restantes;
        if locais.len() == antes {
            break;
        }
    }

    // Servico so entra se algum require herdado comecar por ele.
    let exprs: Vec<String> = requires.iter().map(|r| reescreve_require(&r.expressao)).collect();
    let servicos: Vec<_> = m
        .servicos
        .iter()
        .filter(|s| exprs.iter().any(|e| e.starts_with(&format!("{}.", s.alias))))
        .collect();

    let mut blocos = vec![format!(
        "--!strict\n\
         -- GERADO por modux a partir de {}\n\
         -- NAO EDITAR A MAO: a proxima geracao sobrescreve.\n",
        m.arquivo
    )];

    let mut cabecalho = Vec::new();
    for s in &servicos {
        cabecalho.push(format!(
            "local {} = game:GetService(\"{}\")",
            s.alias, s.servico
        ));
    }
    for r in &requires {
        cabecalho.push(format!(
            "local {} = require({})",
            r.alias,
            reescreve_require(&r.expressao)
        ));
    }
    if !cabecalho.is_empty() {
        blocos.push(format!("{}\n", cabecalho.join("\n")));
    }

    if !locais.is_empty() {
        let copiados: Vec<&str> = locais.iter().map(|t| t.texto.as_str()).collect();
        blocos.push(format!("{}\n", copiados.join("\n")));
    }

    let mut corpo = vec!["export type Public = {".to_string()];
    for (nome, tipo) in &membros {
        corpo.push(format!("\t{nome}: {tipo},"));
    }
    corpo.push("}".to_string());
    blocos.push(format!("{}\n", corpo.join("\n")));

    blocos.push("return {}\n".to_string());
    blocos.join("\n")
}
