
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Side {
    Client,
    Server,
    Shared,
}

impl Side {
    pub fn name(self) -> &'static str {
        match self {
            Side::Client => "client",
            Side::Server => "server",
            Side::Shared => "shared",
        }
    }

    pub fn sees(self, other: Side) -> bool {
        match (self, other) {
            (_, Side::Shared) => true,
            (Side::Shared, _) => false,
            (a, b) => a == b,
        }
    }
}

const SERVER_ONLY: [&str; 2] = ["ServerScriptService", "ServerStorage"];
const CLIENT_ONLY: [&str; 4] = ["StarterPlayer", "StarterGui", "StarterPack", "ReplicatedFirst"];

pub struct Map {
    entries: BTreeMap<String, Vec<String>>,
}

impl Map {
    pub fn read(project: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(project)
            .with_context(|| format!("{} not found", project.display()))?;
        let json: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", project.display()))?;
        let tree = json
            .get("tree")
            .with_context(|| format!("{} has no `tree`", project.display()))?;

        let mut entries = BTreeMap::new();
        if let Value::Object(map) = tree {
            for (key, child) in map {
                if !key.starts_with('$') {
                    descend(child, &[key.clone()], &mut entries);
                }
            }
        }
        Ok(Self { entries })
    }

    /// Monta o mesmo mapa a partir de um sourcemap, para os fluxos que nao tem
    /// project file.
    ///
    /// O project file da PREFIXOS: "a pasta src/Vital/server vira este ramo",
    /// e o resto do caminho e deduzido do disco. O sourcemap ja vem resolvido,
    /// um no por instancia, com os arquivos que a originaram. Entao aqui cada
    /// arquivo entra como a sua propria entrada, e o casamento por prefixo em
    /// `path` vira um acerto exato — que e o caso mais especifico e, pela
    /// ordenacao por comprimento, o primeiro a ser testado.
    ///
    /// O `init.luau` de uma pasta aparece no sourcemap sob o no da pasta, ja
    /// com o nome certo, entao nao ha caso especial a tratar: o trail e o
    /// caminho final.
    pub fn from_sourcemap(file: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(file)
            .with_context(|| format!("{} not found", file.display()))?;
        let json: Value = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", file.display()))?;

        let mut entries = BTreeMap::new();
        gather(&json, &[], &mut entries);
        if entries.is_empty() {
            bail!("{} has no filePaths; is it a sourcemap?", file.display());
        }
        Ok(Self { entries })
    }

    pub fn path(&self, file: &Path) -> Result<String> {
        let rel = file.to_string_lossy().replace('\\', "/");

        let mut keys: Vec<&String> = self.entries.keys().collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k.len()));

        for disk in keys {
            if rel != *disk && !rel.starts_with(&format!("{disk}/")) {
                continue;
            }
            let remainder = rel[disk.len()..].trim_matches('/');
            let mut parts: Vec<String> =
                remainder.split('/').filter(|p| !p.is_empty()).map(str::to_string).collect();

            if let Some(last) = parts.last().cloned() {
                if let Some(name) = last.strip_suffix(".luau").or(last.strip_suffix(".lua")) {
                    parts.pop();
                    // `Foo.client.luau` vira um LocalScript chamado `Foo`, nao
                    // `Foo.client`: o sufixo escolhe a CLASSE do script e nao
                    // faz parte do nome. Sem descontar, o caminho ganhava um
                    // segmento a mais e nao correspondia a instancia nenhuma.
                    let name = name.strip_suffix(".client").unwrap_or(name);
                    let name = name.strip_suffix(".server").unwrap_or(name);
                    if name != "init" {
                        parts.push(name.to_string());
                    }
                }
            }

            let mut trail = self.entries[disk].clone();
            trail.extend(parts);
            return Ok(trail.join("."));
        }

        bail!("path outside default.project.json: {rel}")
    }

    pub fn side(&self, file: &Path) -> Result<Side> {
        let path = self.path(file)?;
        let root = path.split('.').next().unwrap_or_default();
        Ok(if SERVER_ONLY.contains(&root) {
            Side::Server
        } else if CLIENT_ONLY.contains(&root) {
            Side::Client
        } else {
            Side::Shared
        })
    }
}

/// Percorre o sourcemap acumulando arquivo -> caminho no DataModel.
///
/// A raiz e o proprio DataModel e nao entra no trail: um caminho comeca em
/// `ServerScriptService`, nao em `NomeDoJogo.ServerScriptService`, senao
/// `side` olharia o segmento errado e tudo viraria Shared.
fn gather(node: &Value, trail: &[String], out: &mut BTreeMap<String, Vec<String>>) {
    let Value::Object(map) = node else { return };
    let Some(name) = map.get("name").and_then(Value::as_str) else { return };

    let mut below = trail.to_vec();
    if !trail.is_empty() || map.get("className").and_then(Value::as_str) != Some("DataModel") {
        below.push(name.to_string());
    }

    if let Some(Value::Array(files)) = map.get("filePaths") {
        for file in files.iter().filter_map(Value::as_str) {
            let disk = file.replace('\\', "/").trim_end_matches('/').to_string();
            // Um `.model.json` ou `.meta.json` ao lado do script aponta para o
            // mesmo no. O que interessa aqui e o arquivo de codigo.
            if !disk.ends_with(".luau") && !disk.ends_with(".lua") {
                continue;
            }
            out.insert(disk.clone(), below.clone());

            // A pasta tambem, quando o arquivo e o init dela. Sem isto, uma
            // folha que o gerador AINDA VAI escrever nao teria como ser
            // resolvida: ela nao esta no sourcemap, porque ainda nao existe no
            // disco. Com a pasta registrada, o casamento por prefixo de `path`
            // encontra `.../VitalService/` e completa com `Type`.
            if let Some(dir) = disk.strip_suffix("/init.luau").or(disk.strip_suffix("/init.lua")) {
                out.insert(dir.to_string(), below.clone());
            }
        }
    }

    if let Some(Value::Array(children)) = map.get("children") {
        for child in children {
            gather(child, &below, out);
        }
    }
}

fn descend(node: &Value, trail: &[String], out: &mut BTreeMap<String, Vec<String>>) {
    let Value::Object(map) = node else { return };

    if let Some(path) = map.get("$path") {
        let text = match path {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o
                .get("optional")
                .or_else(|| o.get("path"))
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        };
        if let Some(t) = text {
            out.insert(t.replace('\\', "/").trim_end_matches('/').to_string(), trail.to_vec());
        }
    }

    for (key, child) in map {
        if key.starts_with('$') {
            continue;
        }
        let mut below = trail.to_vec();
        below.push(key.clone());
        descend(child, &below, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escrever(dir: &Path, nome: &str, texto: &str) -> std::path::PathBuf {
        let p = dir.join(nome);
        std::fs::write(&p, texto).unwrap();
        p
    }

    /// O sourcemap tem que produzir os mesmos caminhos que o project file,
    /// senao a flag trocaria o mapa por outro parecido e o erro apareceria no
    /// Manifest, longe daqui.
    #[test]
    fn sourcemap_concorda_com_o_project_file() {
        let dir = std::env::temp_dir().join("modux-rojo-teste");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let projeto = escrever(
            &dir,
            "default.project.json",
            r#"{
              "name": "Jogo",
              "tree": {
                "$className": "DataModel",
                "ServerScriptService": {
                  "server": { "Vital": { "$path": "src/Vital/server" } }
                },
                "StarterPlayer": {
                  "StarterPlayerScripts": {
                    "client": { "Input": { "$path": "src/Input/client" } }
                  }
                },
                "ReplicatedStorage": {
                  "shared": { "Libs": { "$path": "src/Libs" } }
                }
              }
            }"#,
        );

        let mapa = escrever(
            &dir,
            "sourcemap.json",
            r#"{
              "name": "Jogo", "className": "DataModel",
              "children": [
                { "name": "ServerScriptService", "className": "ServerScriptService", "children": [
                  { "name": "server", "className": "Folder", "children": [
                    { "name": "Vital", "className": "Folder", "children": [
                      { "name": "VitalService", "className": "ModuleScript",
                        "filePaths": ["src/Vital/server/VitalService/init.luau"],
                        "children": [
                          { "name": "Type", "className": "ModuleScript",
                            "filePaths": ["src/Vital/server/VitalService/Type.luau"] }
                        ] }
                    ] }
                  ] }
                ] },
                { "name": "StarterPlayer", "className": "StarterPlayer", "children": [
                  { "name": "StarterPlayerScripts", "className": "StarterPlayerScripts", "children": [
                    { "name": "client", "className": "Folder", "children": [
                      { "name": "Input", "className": "Folder", "children": [
                        { "name": "InputController", "className": "ModuleScript",
                          "filePaths": ["src/Input/client/InputController/init.luau"] }
                      ] }
                    ] }
                  ] }
                ] },
                { "name": "ReplicatedStorage", "className": "ReplicatedStorage", "children": [
                  { "name": "shared", "className": "Folder", "children": [
                    { "name": "Libs", "className": "Folder", "children": [
                      { "name": "Charm", "className": "ModuleScript",
                        "filePaths": ["src/Libs/Charm.luau"] }
                    ] }
                  ] }
                ] }
              ]
            }"#,
        );

        let a = Map::read(&projeto).unwrap();
        let b = Map::from_sourcemap(&mapa).unwrap();

        for arquivo in [
            "src/Vital/server/VitalService/init.luau",
            "src/Vital/server/VitalService/Type.luau",
            "src/Input/client/InputController/init.luau",
            "src/Libs/Charm.luau",
        ] {
            let p = Path::new(arquivo);
            assert_eq!(
                a.path(p).unwrap(),
                b.path(p).unwrap(),
                "caminhos divergem para {arquivo}"
            );
            assert_eq!(a.side(p).unwrap(), b.side(p).unwrap(), "lado diverge para {arquivo}");
        }

        // A folha que ainda nao existe: o sourcemap nao a conhece, e mesmo
        // assim tem que resolver, pela pasta do init.
        let nova = Path::new("src/Input/client/InputController/Type.luau");
        assert_eq!(a.path(nova).unwrap(), b.path(nova).unwrap());
        assert_eq!(
            b.path(nova).unwrap(),
            "StarterPlayer.StarterPlayerScripts.client.Input.InputController.Type"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_raiz_do_sourcemap_nao_entra_no_caminho() {
        let dir = std::env::temp_dir().join("modux-rojo-raiz");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mapa = escrever(
            &dir,
            "sourcemap.json",
            r#"{ "name": "NomeDoJogo", "className": "DataModel", "children": [
                 { "name": "ServerScriptService", "className": "ServerScriptService", "children": [
                   { "name": "Foo", "className": "ModuleScript",
                     "filePaths": ["src/Foo/init.luau"] } ] } ] }"#,
        );
        let m = Map::from_sourcemap(&mapa).unwrap();
        let p = Path::new("src/Foo/init.luau");
        // Se o nome do jogo entrasse, o primeiro segmento nao seria
        // ServerScriptService e o lado cairia em Shared.
        assert_eq!(m.path(p).unwrap(), "ServerScriptService.Foo");
        assert_eq!(m.side(p).unwrap(), Side::Server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
