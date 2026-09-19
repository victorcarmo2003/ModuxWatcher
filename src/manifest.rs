
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::extract::Module;
use crate::rojo::{Side, Map};


/// Onde o framework mora no disco.
///
/// Com o rogen, a arvore do jogo e DERIVADA das pastas: `src/Modux/client`
/// vira `StarterPlayerScripts.client.Modux`, e o project file guarda essa
/// traducao. Num sync Studio-first nao ha traducao nenhuma — o disco JA E a
/// arvore —, entao os mesmos arquivos precisam nascer no caminho inteiro.
///
/// As duas formas apontam para as mesmas instancias no fim, e e isso que
/// importa: `require(ServerScriptService.server.Modux)` continua valendo nos
/// dois fluxos, sem ninguem reescrever require nenhum.
pub struct Layout {
    pub source: PathBuf,
    pub targets: Vec<(Side, PathBuf)>,
    pub module_lists: Vec<(Side, PathBuf)>,
    pub libs_dir: PathBuf,
    pub libs_target: PathBuf,
}

impl Layout {
    /// O disco descreve pastas e o rogen traduz para a arvore.
    pub fn rogen(source: &Path) -> Self {
        Self {
            source: source.to_path_buf(),
            targets: vec![
                (Side::Client, source.join("Modux/client/Manifest/init.luau")),
                (Side::Server, source.join("Modux/server/Manifest/init.luau")),
            ],
            module_lists: vec![
                (Side::Client, source.join("Modux/client/Modules.luau")),
                (Side::Server, source.join("Modux/server/Modules.luau")),
            ],
            libs_dir: source.join("Libs"),
            libs_target: source.join("Modux/shared/Libs.luau"),
        }
    }

    /// O disco E a arvore, entao o caminho carrega o servico inteiro.
    pub fn mirror(source: &Path) -> Self {
        let client = source.join("StarterPlayer/StarterPlayerScripts/client");
        let server = source.join("ServerScriptService/server");
        let shared = source.join("ReplicatedStorage/shared");
        Self {
            source: source.to_path_buf(),
            targets: vec![
                (Side::Client, client.join("Modux/Manifest/init.luau")),
                (Side::Server, server.join("Modux/Manifest/init.luau")),
            ],
            module_lists: vec![
                (Side::Client, client.join("Modux/Modules.luau")),
                (Side::Server, server.join("Modux/Modules.luau")),
            ],
            libs_dir: shared.join("Libs"),
            libs_target: shared.join("Modux/Libs.luau"),
        }
    }
}

fn buckets(side: Side) -> &'static [(&'static str, &'static str)] {
    match side {
        Side::Client => &[("Controller", "AllControllers"), ("Component", "AllComponents")],
        Side::Server => &[("Service", "AllServices"), ("Component", "AllComponents")],
        Side::Shared => &[],
    }
}


pub fn validate(modules: &[Module], map: &Map) -> Result<BTreeMap<String, Side>> {
    let mut by_id: BTreeMap<&str, &Module> = BTreeMap::new();
    for m in modules {
        if let Some(previous) = by_id.insert(m.id.as_str(), m) {
            bail!("duplicate ID {:?} in {} and {}", m.id, previous.file, m.file);
        }
    }

    // Dois modulos na mesma pasta escreveriam o mesmo Type.luau, e o segundo
    // sobrescreve o primeiro sem reclamar. O Manifest fica apontando os dois
    // IDs para a folha de um so, entao um modulo passa a ter os metodos do
    // outro e o erro nao aparece em lugar nenhum: o tipo esta errado, nao
    // ausente. Por isso isto e um bail, nao um warning.
    let mut leaves: BTreeMap<PathBuf, &Module> = BTreeMap::new();
    for m in modules {
        let leaf = crate::emit::leaf_path(Path::new(&m.file));
        if let Some(previous) = leaves.insert(leaf.clone(), m) {
            let shown = leaf.to_string_lossy().replace('\\', "/");
            bail!(
                "{} and {} would both write {}.\n  \
                 A module needs a folder of its own, with the code in init.luau, \
                 because the type leaf is written beside it.\n  \
                 Run `modux fix` to move each one into its own folder.",
                previous.file,
                m.file,
                shown
            );
        }
    }

    let mut sides: BTreeMap<String, Side> = BTreeMap::new();
    for m in modules {
        sides.insert(m.id.clone(), map.side(Path::new(&m.file))?);
    }

    for m in modules {
        let mine = sides[&m.id];
        for dep in &m.declared_require {
            let Some(target) = by_id.get(dep.as_str()) else {
                let available: Vec<&str> = by_id.keys().copied().collect();
                bail!(
                    "[Manifest] {} requires {:?}, which does not exist. Available: {}",
                    m.file,
                    dep,
                    available.join(", ")
                );
            };
            let theirs = sides[dep.as_str()];
            if !mine.sees(theirs) {
                bail!(
                    "[Manifest] {} is {} and requires {:?}, which is {}.\n  \
                     {} cannot see {} — move one of them, or go through the network.\n  \
                     ({} is in {})",
                    m.file,
                    mine.name(),
                    dep,
                    theirs.name(),
                    mine.name(),
                    theirs.name(),
                    dep,
                    target.file
                );
            }
        }
    }

    Ok(sides)
}

pub fn emit(
    side: Side,
    modules: &[Module],
    sides: &BTreeMap<String, Side>,
    map: &Map,
) -> Result<Option<String>> {
    let visible: Vec<&Module> = modules
        .iter()
        .filter(|m| side.sees(sides[&m.id]))
        .collect();

    let mut blocks = vec!["--!strict\n".to_string()];

    let mut paths = vec![
    ];
    for m in &visible {
        let leaf = crate::emit::leaf_path(Path::new(&m.file));
        paths.push((m.id.clone(), map.path(&leaf)?));
    }

    let mut roots: Vec<String> = Vec::new();
    for (_, path) in &paths {
        let root = path.split('.').next().unwrap_or_default().to_string();
        if !root.is_empty() && !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots.sort();

    let mut requires: Vec<String> = roots
        .iter()
        .map(|r| format!("local {r} = game:GetService(\"{r}\")"))
        .collect();
    for (alias, path) in &paths {
        requires.push(format!("local {alias} = require({path})"));
    }
    blocks.push(format!("{}\n", requires.join("\n")));

    for (kind, alias) in buckets(side) {
        let of_kind: Vec<&&Module> = visible.iter().filter(|m| m.kind == *kind).collect();
        if of_kind.is_empty() {
            blocks.push(format!("export type {alias} = {{}}\n"));
            continue;
        }
        let mut body = vec![format!("export type {alias} = {{")];
        for m in of_kind {
            body.push(format!("\t{}: {},", m.id, entry(m)));
        }
        body.push("}".to_string());
        blocks.push(format!("{}\n", body.join("\n")));
    }

    blocks.push(component_access(&visible));

    blocks.push("return {}\n".to_string());
    Ok(Some(blocks.join("\n")))
}

// One concrete entry per component, rather than a generic
// `GetComponent<CID>(id: CID, ...) -> index<AllComponents, CID>`.
//
// That generic form does typecheck on its own, but it has to reach the module
// author through the extras of `SelfOf.Build`, and a generic function there
// stops the type function from reducing. It does not error: `Build<...>` simply
// stays unevaluated, and the symptom lands somewhere else entirely, as
// `Cannot add property 'Whatever' to table 'setmetatable<Build<...>, ...>'` on
// a method declaration that was fine a moment ago.
//
// Writing the table out per component keeps the lookup by key with no generic
// anywhere, which is work this generator is here to do.
fn component_access(visible: &[&Module]) -> String {
    let components: Vec<&&Module> = visible.iter().filter(|m| m.kind == "Component").collect();
    if components.is_empty() {
        return "export type ComponentAccess = {}\n".to_string();
    }

    let mut body = vec!["export type ComponentAccess = {".to_string()];
    for m in components {
        let public = entry(m);
        body.push(format!("\t{}: {{", m.id));
        body.push(format!(
            "\t\tGet: (self: any, instance: Instance) -> {public}?,"
        ));
        body.push(format!(
            "\t\tCreate: (self: any, instance: Instance) -> {public},"
        ));
        body.push(format!("\t\tAll: (self: any) -> {{ {public} }},"));
        // Create exists so nobody has to touch the tag by hand; Destroy has to
        // exist for the same reason, or the only way back out is RemoveTag.
        body.push("\t\tDestroy: (self: any, instance: Instance) -> (),".to_string());
        body.push("\t},".to_string());
    }
    body.push("}".to_string());
    format!("{}\n", body.join("\n"))
}

fn entry(m: &Module) -> String {
    format!("{}.Public", m.id)
}

pub fn emit_module_list(
    side: Side,
    modules: &[Module],
    sides: &BTreeMap<String, Side>,
    map: &Map,
) -> Result<Option<String>> {
    let visible: Vec<&Module> = modules.iter().filter(|m| side.sees(sides[&m.id])).collect();

    let mut paths = Vec::new();
    for m in &visible {
        paths.push(map.path(Path::new(&m.file))?);
    }

    let mut roots: Vec<String> = Vec::new();
    for path in &paths {
        let root = path.split('.').next().unwrap_or_default().to_string();
        if !root.is_empty() && !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots.sort();

    let _ = side;
    let mut out = String::from("--!strict\n\n");
    for r in &roots {
        out.push_str(&format!("local {r} = game:GetService(\"{r}\")\n"));
    }
    out.push_str("\nreturn {\n");
    for path in &paths {
        out.push_str(&format!("\t{path},\n"));
    }
    out.push_str("} :: { ModuleScript }\n");
    Ok(Some(out))
}


// Libs sao dependencia de DADOS, nao de codigo. O core nao requer Promise nem
// Signal nem rede; ele requer este arquivo, que o gerador escreve a partir do
// que existe em src/Libs. Apagar uma lib nao quebra o framework: a entrada
// some do tipo e quem a usava falha com "nao existe em Api", no lugar certo.
//
// `typeof(X)` em vez de `X.Api` de proposito: assim a lib nao precisa aderir a
// contrato nenhum para entrar. Promise exporta PromiseAPI, Signal exporta Api,
// e nenhum dos dois precisou mudar.
pub fn emit_libs(root: &Path, dir: &Path, map: &Map) -> Result<String> {
    let dir = root.join(dir);
    let mut libs: Vec<(String, String)> = Vec::new();

    if dir.is_dir() {
        let mut entradas: Vec<_> = std::fs::read_dir(&dir)
            .with_context(|| format!("could not read {}", dir.display()))?
            .filter_map(Result::ok)
            .collect();
        entradas.sort_by_key(std::fs::DirEntry::path);

        for entrada in entradas {
            let caminho = entrada.path();
            let (nome, alvo) = if caminho.is_dir() {
                let init = caminho.join("init.luau");
                if !init.exists() {
                    continue;
                }
                (caminho.file_name(), init)
            } else if caminho.extension().and_then(|e| e.to_str()) == Some("luau") {
                (caminho.file_stem(), caminho.clone())
            } else {
                continue;
            };
            let Some(nome) = nome.and_then(|n| n.to_str()).map(str::to_string) else {
                continue;
            };
            let rel = alvo.strip_prefix(root).unwrap_or(&alvo).to_path_buf();
            libs.push((nome, map.path(&rel)?));
        }
    }

    let mut blocos = vec!["--!strict\n".to_string()];

    if libs.is_empty() {
        blocos.push("export type Api = {}\n".to_string());
        blocos.push("return {} :: Api\n".to_string());
        return Ok(blocos.join("\n"));
    }

    let mut raizes: Vec<String> = Vec::new();
    for (_, caminho) in &libs {
        let raiz = caminho.split('.').next().unwrap_or_default().to_string();
        if !raiz.is_empty() && !raizes.contains(&raiz) {
            raizes.push(raiz);
        }
    }
    raizes.sort();

    let mut requires: Vec<String> = raizes
        .iter()
        .map(|r| format!("local {r} = game:GetService(\"{r}\")"))
        .collect();
    for (nome, caminho) in &libs {
        requires.push(format!("local {nome} = require({caminho})"));
    }
    blocos.push(format!("{}\n", requires.join("\n")));

    let mut tipo = vec!["export type Api = {".to_string()];
    for (nome, _) in &libs {
        tipo.push(format!("\t{nome}: typeof({nome}),"));
    }
    tipo.push("}".to_string());
    blocos.push(format!("{}\n", tipo.join("\n")));

    let mut corpo = vec!["return {".to_string()];
    for (nome, _) in &libs {
        corpo.push(format!("\t{nome} = {nome},"));
    }
    corpo.push("} :: Api".to_string());
    blocos.push(format!("{}\n", corpo.join("\n")));

    Ok(blocos.join("\n"))
}
