//! Escreve o Manifest a partir de todos os modulos do projeto.
//!
//! O Manifest e o unico arquivo que conhece todos os modulos ao mesmo tempo. E
//! dele que o corpo de um modulo puxa o tipo esperado do `self`, e por isso
//! `self` fica tipado sem voce anotar nada.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::extract::Modulo;
use crate::rojo::Mapa;

pub const SELF_OF: &str = "src/Types/shared/SelfOf.luau";
pub const DESTINO: &str = "src/Modux/shared/Manifest/init.luau";
const SERVICO_RAIZ: &str = "ReplicatedStorage";

const ESPECIES: [(&str, &str); 3] = [
    ("Controller", "AllControllers"),
    ("Service", "AllServices"),
    ("Component", "AllComponents"),
];

pub fn destino(raiz: &Path) -> PathBuf {
    raiz.join(DESTINO)
}

pub fn emitir(modulos: &[Modulo], mapa: &Mapa) -> Result<String> {
    let mut por_id: BTreeMap<&str, &Modulo> = BTreeMap::new();
    for m in modulos {
        if let Some(anterior) = por_id.insert(m.id.as_str(), m) {
            bail!(
                "ID duplicado {:?} em {} e {}",
                m.id,
                anterior.arquivo,
                m.arquivo
            );
        }
    }

    // Dependencia para modulo que nao existe: falhar aqui, com a lista, antes
    // de escrever qualquer coisa.
    for m in modulos {
        for dep in &m.dependencias {
            if !por_id.contains_key(dep.as_str()) {
                let disponiveis: Vec<&str> = por_id.keys().copied().collect();
                bail!(
                    "[Manifest] {} pede {:?}, que nao existe. Disponiveis: {}",
                    m.arquivo,
                    dep,
                    disponiveis.join(", ")
                );
            }
        }
    }

    let mut blocos = vec![String::from(
        "--!strict\n\
         -- GERADO por modux a partir das folhas do projeto.\n\
         -- NAO EDITAR A MAO: a proxima geracao sobrescreve.\n\
         --\n\
         -- Cada entrada e o `self` ja enxertado: os membros da folha mais as\n\
         -- `Dependencies` resolvidas. As dependencias apontam para a folha CRUA,\n\
         -- nunca para a enxertada, senao o enxerto desce infinitamente.\n",
    )];

    let mut requires = vec![format!(
        "local {SERVICO_RAIZ} = game:GetService(\"{SERVICO_RAIZ}\")"
    )];
    requires.push(format!(
        "local SelfOf = require({})",
        mapa.caminho(Path::new(SELF_OF))?
    ));
    for (id, m) in &por_id {
        let folha = Path::new(&m.arquivo)
            .parent()
            .unwrap_or(Path::new("."))
            .join("Type.luau");
        requires.push(format!("local {id} = require({})", mapa.caminho(&folha)?));
    }
    blocos.push(format!("{}\n", requires.join("\n")));

    for (especie, alias) in ESPECIES {
        let do_tipo: Vec<&&Modulo> = por_id
            .values()
            .filter(|m| m.especie == especie)
            .collect();

        if do_tipo.is_empty() {
            blocos.push(format!("export type {alias} = {{}}\n"));
            continue;
        }

        let mut corpo = vec![format!("export type {alias} = {{")];
        for m in do_tipo {
            let deps = if m.dependencias.is_empty() {
                "{}".to_string()
            } else {
                let itens: Vec<String> = m
                    .dependencias
                    .iter()
                    .map(|d| format!("{d}: {d}.Public"))
                    .collect();
                format!("{{ {} }}", itens.join(", "))
            };
            corpo.push(format!(
                "\t{}: SelfOf.Build<{}.Public, {deps}>,",
                m.id, m.id
            ));
        }
        corpo.push("}".to_string());
        blocos.push(format!("{}\n", corpo.join("\n")));
    }

    blocos.push("return {}\n".to_string());
    Ok(blocos.join("\n"))
}
