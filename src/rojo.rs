//! Traduz caminho no disco para caminho no DataModel, lendo o projeto Rojo.
//!
//! Nada aqui e hardcoded: se voce mexer no `default.project.json`, o Manifest
//! acompanha. Arquivo fora do mapeamento falha alto, em vez de virar um caminho
//! errado que so quebra em runtime.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

pub struct Mapa {
    /// prefixo no disco -> trilha no DataModel
    entradas: BTreeMap<String, Vec<String>>,
}

impl Mapa {
    pub fn ler(projeto: &Path) -> Result<Self> {
        let texto = std::fs::read_to_string(projeto)
            .with_context(|| format!("nao achei {}", projeto.display()))?;
        let json: Value = serde_json::from_str(&texto)
            .with_context(|| format!("{} nao e JSON valido", projeto.display()))?;
        let arvore = json
            .get("tree")
            .with_context(|| format!("{} nao tem `tree`", projeto.display()))?;

        let mut entradas = BTreeMap::new();
        if let Value::Object(mapa) = arvore {
            for (chave, filho) in mapa {
                if !chave.starts_with('$') {
                    desce(filho, &[chave.clone()], &mut entradas);
                }
            }
        }
        Ok(Self { entradas })
    }

    /// `src/Entity/shared/Zombie/Type.luau` -> `...Entity.Zombie.Type`
    pub fn caminho(&self, arquivo: &Path) -> Result<String> {
        let rel = arquivo.to_string_lossy().replace('\\', "/");

        // O prefixo mais longo ganha, senao `src/Modux/shared` perderia para um
        // mapeamento mais raso.
        let mut chaves: Vec<&String> = self.entradas.keys().collect();
        chaves.sort_by_key(|k| std::cmp::Reverse(k.len()));

        for disco in chaves {
            if rel != *disco && !rel.starts_with(&format!("{disco}/")) {
                continue;
            }
            let resto = rel[disco.len()..].trim_matches('/');
            let mut partes: Vec<String> =
                resto.split('/').filter(|p| !p.is_empty()).map(str::to_string).collect();

            if let Some(ultima) = partes.last().cloned() {
                if let Some(nome) = ultima.strip_suffix(".luau") {
                    partes.pop();
                    // `init.luau` vira a propria pasta, igual o Rojo resolve
                    if nome != "init" {
                        partes.push(nome.to_string());
                    }
                }
            }

            let mut trilha = self.entradas[disco].clone();
            trilha.extend(partes);
            return Ok(trilha.join("."));
        }

        bail!("caminho fora do default.project.json: {rel}")
    }
}

fn desce(no: &Value, trilha: &[String], saida: &mut BTreeMap<String, Vec<String>>) {
    let Value::Object(mapa) = no else { return };

    if let Some(caminho) = mapa.get("$path") {
        let texto = match caminho {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o
                .get("optional")
                .or_else(|| o.get("path"))
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        };
        if let Some(t) = texto {
            saida.insert(t.replace('\\', "/").trim_end_matches('/').to_string(), trilha.to_vec());
        }
    }

    for (chave, filho) in mapa {
        if chave.starts_with('$') {
            continue;
        }
        let mut abaixo = trilha.to_vec();
        abaixo.push(chave.clone());
        desce(filho, &abaixo, saida);
    }
}
