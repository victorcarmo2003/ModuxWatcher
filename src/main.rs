
mod ast;
mod emit;
mod extract;
mod manifest;
mod rojo;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use walkdir::WalkDir;

use extract::{Extractor, Module};
use rojo::Map;

const PROJECT: &str = "default.project.json";
const SOURCE: &str = "src";
const MIRROR_SOURCE: &str = "sync";
const DEFAULT_INTERVAL_MS: u64 = 400;

#[derive(Parser)]
#[command(
    name = "modux",
    version,
    about = "Type generator for the Modux Roblox framework",
    long_about = "Reads the project modules and writes the per-module type \
                  leaves (Type.luau) and the Manifest.\n\n\
                  Parsing is built in, so there is nothing to install alongside \
                  this binary."
)]
struct Cli {
    #[arg(long, short, global = true, value_name = "PATH")]
    project: Option<PathBuf>,

    /// Folder the generator scans. Defaults to `src`, or to `sync` with --sourcemap.
    #[arg(long, global = true, value_name = "DIR")]
    source: Option<PathBuf>,

    /// Take the instance tree from a sourcemap instead of default.project.json.
    ///
    /// For flows with no project file, such as a Studio-first sync tool that
    /// emits its own sourcemap. The generator stops reading the project file
    /// and stops rebuilding the sourcemap, because it is no longer the owner.
    #[arg(long, global = true, value_name = "PATH")]
    sourcemap: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write the type leaves and the Manifest once, then exit.
    Generate,

    /// Regenerate on every source change until interrupted.
    Watch {
        /// Milliseconds between filesystem polls.
        #[arg(long, default_value_t = DEFAULT_INTERVAL_MS)]
        interval: u64,

        /// Flatten a newly created `Foo/init.luau` into `Foo.luau` and reopen it.
        #[arg(long)]
        fix: bool,

        /// Rewrite the sourcemap after a leaf changes, to wake the language server.
        #[arg(long)]
        nudge: bool,
    },

    /// Fail if anything on disk differs from what Generate would write.
    Check,

    /// Print every module found, with its id, kind and side.
    List,

    /// Flatten every module that still lives alone inside a folder of its own.
    Fix {
        /// Report what would move, without touching anything.
        #[arg(long)]
        dry_run: bool,

        /// Reopen each moved module in VS Code.
        #[arg(long)]
        open: bool,
    },

    /// Dump the extracted shape of one module as JSON, for debugging.
    Extract {
        /// Module to inspect.
        file: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(&cli) {
        eprintln!("modux: {err:#}");
        std::process::exit(1);
    }
}

/// De onde vem a arvore de instancias.
///
/// O project file descreve pastas e o gerador deduz o resto; o sourcemap ja
/// vem resolvido. Os dois produzem o mesmo `Map` — ha teste comparando os dois
/// caminho por caminho —, mas quem os mantem atualizados e diferente, e por
/// isso a fonte precisa viajar junto: o project file e reescrito pelo rogen e
/// o sourcemap pertence a quem o emite.
#[derive(Clone)]
enum Source {
    Project(PathBuf),
    Sourcemap(PathBuf),
}

impl Source {
    fn from(cli: &Cli, root: &Path) -> Self {
        match &cli.sourcemap {
            Some(p) if p.is_absolute() => Source::Sourcemap(p.clone()),
            Some(p) => Source::Sourcemap(root.join(p)),
            None => Source::Project(root.join(PROJECT)),
        }
    }

    fn file(&self) -> &Path {
        match self {
            Source::Project(p) | Source::Sourcemap(p) => p,
        }
    }

    fn read(&self) -> Result<Map> {
        match self {
            Source::Project(p) => Map::read(p),
            Source::Sourcemap(p) => Map::from_sourcemap(p),
        }
    }

    /// Refazer o sourcemap so faz sentido quando ele e derivado do project
    /// file. Quando ELE e a fonte, quem o mantem e outro processo, e chamar o
    /// rojo aqui sobrescreveria o mapa alheio com um vindo de um project file
    /// que talvez nem exista.
    fn owns_sourcemap(&self) -> bool {
        matches!(self, Source::Project(_))
    }

    /// O project file implica o rogen traduzindo pastas; o sourcemap implica
    /// que o disco ja e a arvore. Sao nomes diferentes para a mesma pergunta,
    /// mas quem le `layout_for` esta perguntando pelo layout, nao por quem
    /// mantem o sourcemap.
    fn is_project(&self) -> bool {
        matches!(self, Source::Project(_))
    }
}

/// O layout acompanha a fonte do mapa porque os dois descrevem a mesma coisa
/// por caminhos diferentes, e misturar daria um gerador varrendo uma arvore e
/// escrevendo noutra. Quem usa sourcemap esta num sync Studio-first, onde o
/// disco ja e a arvore; quem usa project file esta com o rogen traduzindo.
fn layout_for(cli: &Cli, source: &Source) -> manifest::Layout {
    let pasta = cli.source.clone().unwrap_or_else(|| {
        PathBuf::from(if source.is_project() { SOURCE } else { MIRROR_SOURCE })
    });
    if source.is_project() {
        manifest::Layout::rogen(&pasta)
    } else {
        manifest::Layout::mirror(&pasta)
    }
}

fn run(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Extract { file } => {
            let data = Extractor::new(file)?.run()?;
            println!("{}", serde_json::to_string_pretty(&data)?);
            Ok(())
        }
        Command::Generate => {
            let root = project_root(cli)?;
            let source = Source::from(cli, &root);
            let mut state = State::new(&root, &source, layout_for(cli, &source))?;
            let started = Instant::now();
            let changed = state.pass(true)?;
            if !changed {
                log("up to date");
            }
            log(&format!("{} ms", started.elapsed().as_millis()));
            Ok(())
        }
        Command::Check => {
            let root = project_root(cli)?;
            let source = Source::from(cli, &root);
            let mut state = State::new(&root, &source, layout_for(cli, &source))?;
            let stale = state.check_stale()?;
            if stale.is_empty() {
                log("up to date");
                return Ok(());
            }
            for p in &stale {
                eprintln!("stale: {}", state.rel(p));
            }
            bail!(
                "{} file(s) out of date. Run `modux generate`.",
                stale.len()
            )
        }
        Command::List => {
            let root = project_root(cli)?;
            let source = Source::from(cli, &root);
            let mut state = State::new(&root, &source, layout_for(cli, &source))?;
            for m in state.modules()? {
                // What the module declared, not what it was seen using. The
                // inferred `dependencies` only covers reads inside a declared
                // method, so a Require used from OnInit would go missing here
                // and the listing would read as if it had not been declared.
                let deps = if m.declared_require.is_empty() {
                    "-".to_string()
                } else {
                    m.declared_require.join(", ")
                };
                println!("{:<12} {:<10} {}  deps: {}", m.id, m.kind, m.file, deps);
            }
            Ok(())
        }
        Command::Fix { dry_run, open } => {
            let root = project_root(cli)?;
            let source = Source::from(cli, &root);
            fix(&root, &source, layout_for(cli, &source), *dry_run, *open)
        }
        Command::Watch { interval, fix: autofix_on, nudge } => {
            let root = project_root(cli)?;
            let source = Source::from(cli, &root);
            let mut state = State::new(&root, &source, layout_for(cli, &source))?;
            let started = Instant::now();
            if !state.pass(true)? {
                log("up to date");
            }
            log(&format!("primeira pass em {} ms", started.elapsed().as_millis()));
            if *autofix_on {
                let foldered = foldered_modules(&root, &state).len();
                if foldered > 0 {
                    log(&format!(
                        "{foldered} module(s) still in a folder of their own; run `modux fix` for those.                          From here on, a new one is flattened as it appears"
                    ));
                }
            }
            watch_loop(&mut state, &source, Duration::from_millis(*interval), *autofix_on, *nudge)
        }
    }
}

fn project_root(cli: &Cli) -> Result<PathBuf> {
    // Com --sourcemap nao ha project file para procurar, e exigir um seria o
    // contrario do que a flag existe para permitir. A raiz passa a ser a pasta
    // que contem o sourcemap, porque os caminhos dentro dele sao relativos a
    // ela.
    if let Some(mapa) = &cli.sourcemap {
        if let Some(p) = &cli.project {
            return Ok(p.clone());
        }
        if !mapa.exists() {
            bail!("{} not found", mapa.display());
        }
        return match mapa.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(p) => Ok(p.to_path_buf()),
            None => std::env::current_dir().context("could not read the current directory"),
        };
    }

    if let Some(p) = &cli.project {
        if !p.join(PROJECT).exists() {
            bail!("{} has no {PROJECT}", p.display());
        }
        return Ok(p.clone());
    }
    let mut current = std::env::current_dir().context("could not read the current directory")?;
    loop {
        if current.join(PROJECT).exists() {
            return Ok(current);
        }
        if !current.pop() {
            bail!(
                "{PROJECT} not found here or in any parent directory.\n\
                 Run this inside the project, pass --project PATH, or use \
                 --sourcemap PATH when there is no project file."
            );
        }
    }
}

/// Achata `Foo/init.luau` de volta em `Foo.luau`.
///
/// Ate 0.6.11 esta funcao fazia o CONTRARIO, e por um motivo que deixou de
/// existir: a folha de tipo era escrita ao lado do modulo, entao dois modulos
/// soltos na mesma pasta disputavam o mesmo `Type.luau` e a pasta propria era
/// a unica saida. Desde a 0.7.0 a folha mora em `ModuxTypes/<lado>/<Id>.luau`,
/// endereçada por ID, e a pasta perdeu a razao de ser: uma pasta cujo unico
/// conteudo e o `init.luau` so acrescenta um nivel na arvore.
///
/// A troca e segura nos dois sentidos, e pelo mesmo motivo: o Rojo transforma
/// uma pasta com `init.luau` num ModuleScript com o nome da pasta, que e
/// exatamente a mesma instancia que `Foo.luau` produz no mesmo pai. `script` e
/// `script.Parent` continuam apontando para onde apontavam. Nenhum require
/// precisa ser reescrito.
///
/// So achata quando a pasta nao tem mais nada dentro alem do `init.luau` e,
/// possivelmente, um `Type.luau` sobrando de antes da migracao (esse e output
/// do proprio gerador, e agora e escrito em outro lugar — sai junto). Qualquer
/// outro arquivo la dentro e coisa que a pessoa pos, e ai a pasta fica.
fn flatten(init: &Path) -> Result<PathBuf> {
    let folder = init
        .parent()
        .with_context(|| format!("{} has no parent folder", init.display()))?;

    let mut leftovers = Vec::new();
    for entry in std::fs::read_dir(folder)
        .with_context(|| format!("could not read {}", folder.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name != "init.luau" && name != "Type.luau" {
            leftovers.push(name.to_string());
        }
    }
    if !leftovers.is_empty() {
        bail!(
            "{} still holds {} — left as a folder on purpose",
            folder.display(),
            leftovers.join(", ")
        );
    }

    let destination = folder.with_extension("luau");
    if destination.exists() {
        bail!("cannot flatten {}: {} already exists", folder.display(), destination.display());
    }

    std::fs::rename(init, &destination)
        .with_context(|| format!("could not move {}", init.display()))?;

    // A folha antiga e output do gerador e agora vive em Types/. Sai com a
    // pasta; se ficasse, `is_generated` a trataria como lixo para sempre.
    let stale = folder.join("Type.luau");
    if stale.exists() {
        let _ = std::fs::remove_file(&stale);
    }
    let _ = std::fs::remove_dir(folder);

    Ok(destination)
}

/// Reabre o arquivo no VS Code. Silencioso de proposito: sem o `code` no PATH
/// o caminho impresso no log continua servindo, e falhar aqui nao e motivo para
/// derrubar uma geracao que deu certo.
fn reopen(path: &Path) {
    let _ = std::process::Command::new("code")
        .arg("--reuse-window")
        .arg(path)
        .status();
}

/// Folhas de antes da 0.7.0: o `Type.luau` que ficou ao lado do modulo.
///
/// O `flatten` so remove a folha antiga da pasta que ele achata. Um modulo que
/// MANTEVE a pasta (porque guarda um irmao de verdade, como o ProfileService
/// do template com o seu Template.luau) fica com a folha velha no disco para
/// sempre — e `is_generated` a ignora pelo nome, entao o gerador nunca mais
/// olha para ela.
///
/// Isso e pior que um arquivo a toa: ela continua sendo um ModuleScript valido
/// no DataModel, com uma copia DESATUALIZADA do tipo que ninguem atualiza. O
/// nome `Type.luau` sempre foi reservado para output do gerador (e o que
/// `is_generated` assume desde antes desta mudanca), entao remove-la na
/// migracao e legitimo.
fn legacy_leaves(root: &Path, state: &State) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = find_modules(root, &state.layout.source, &state.paths())
        .into_iter()
        .filter_map(|m| {
            let leaf = m.parent()?.join("Type.luau");
            leaf.exists().then_some(leaf)
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Modulos escritos como `Foo/init.luau` numa pasta que nao guarda mais nada.
/// Desde a 0.7.0 essa pasta e um nivel a toa (ver `flatten`).
fn foldered_modules(root: &Path, state: &State) -> Vec<PathBuf> {
    find_modules(root, &state.layout.source, &state.paths())
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n == "init.luau"))
        .collect()
}

fn fix(root: &Path, source: &Source, layout: manifest::Layout, dry_run: bool, open: bool) -> Result<()> {
    let state = State::new(root, source, layout)?;
    let foldered = foldered_modules(root, &state);
    let legacy = legacy_leaves(root, &state);

    let antiga_existe = root.join(&state.layout.source).join("Types").is_dir();
    if foldered.is_empty() && legacy.is_empty() && !antiga_existe {
        log("every module is already a file of its own");
        return Ok(());
    }

    if dry_run {
        for file in &foldered {
            let Some(folder) = file.parent() else { continue };
            println!("{}  ->  {}", state.rel(file), state.rel(&folder.with_extension("luau")));
        }
        for leaf in &legacy {
            println!("{}  ->  (removida)", state.rel(leaf));
        }
        return Ok(());
    }

    for file in &foldered {
        match flatten(file) {
            Ok(destination) => {
                log(&format!("{}  ->  {}", state.rel(file), state.rel(&destination)));
                if open {
                    reopen(&destination);
                }
            }
            // Pasta com conteudo proprio nao e erro de migracao, e escolha de
            // quem escreveu: reportar e seguir vale mais que abortar o lote.
            Err(err) => log(&format!("kept {}: {err:#}", state.rel(file))),
        }
    }

    // Pasta `Types/` da 0.7.0/0.7.1, antes do nome mudar para `ModuxTypes/`
    // (ver Layout::types_dirs: `src/Types/shared` caia em cima de
    // `src/Shared/Types` do framework). Tudo la dentro e output do gerador.
    let antiga = root.join(&state.layout.source).join("Types");
    if antiga.is_dir() {
        match std::fs::remove_dir_all(&antiga) {
            Ok(()) => log(&format!("pasta {} da 0.7.0/0.7.1 removida", state.rel(&antiga))),
            Err(err) => log(&format!("could not remove {}: {err:#}", state.rel(&antiga))),
        }
    }

    // Depois dos flattens: um modulo achatado ja levou a sua folha junto, e o
    // que sobra aqui e de quem manteve a pasta.
    for leaf in legacy_leaves(root, &state) {
        match std::fs::remove_file(&leaf) {
            Ok(()) => log(&format!("folha anterior a 0.7.0 removida: {}", state.rel(&leaf))),
            Err(err) => log(&format!("could not remove {}: {err:#}", state.rel(&leaf))),
        }
    }

    log("run `modux generate` to write the leaves in their new place,");
    log("then `rogen build` again so ModuxTypes/ enters the project file");
    Ok(())
}

/// Um modulo que acabou de aparecer dentro de uma pasta so dele. Roda so
/// dentro do watch, e so depois da primeira pass: ali o cache ja conhece tudo
/// que existia antes, entao um caminho ausente dele e de fato novo — o arquivo
/// que a pessoa acabou de criar. Achatar o projeto inteiro sem pedir seria
/// outra coisa, e para isso existe `modux fix`.
fn autofix(state: &mut State) -> bool {
    let root = state.root.clone();
    let foldered = foldered_modules(&root, state);
    let fresh: Vec<PathBuf> = foldered
        .iter()
        .filter(|p| !state.cache.contains_key(*p))
        .cloned()
        .collect();

    let mut did = false;
    for file in &fresh {
        match flatten(file) {
            Ok(destination) => {
                log(&format!("moved: {}  ->  {}", state.rel(file), state.rel(&destination)));
                reopen(&destination);
                did = true;
            }
            Err(err) => log(&format!("could not move {}: {err:#}", state.rel(file))),
        }
    }
    did
}

/// `alvos` ja vem canonicalizado (ver `canonicalizar`): esta funcao roda uma
/// vez por arquivo `.luau` da arvore, e refazer o canonicalize dos alvos aqui
/// dentro custava O(arquivos x alvos) chamadas de sistema.
fn is_generated(path: &Path, alvos: &[PathBuf]) -> bool {
    // Rede de seguranca para projeto anterior a 0.7.0: a folha morava ao lado
    // do modulo e pode ter sobrado no disco depois da migracao. Nunca deve ser
    // confundida com um modulo.
    if path.file_name().is_some_and(|n| n == "Type.luau") {
        return true;
    }
    let Ok(a) = path.canonicalize() else { return false };
    // `starts_with` alem de `==` porque `paths()` agora entrega tambem as
    // PASTAS de tipo, e o que interessa e tudo que esta dentro delas.
    alvos.iter().any(|b| a == *b || a.starts_with(b))
}

/// Resolve os alvos uma vez so, para `is_generated` nao refazer isso por
/// arquivo. Alvo que ainda nao existe no disco simplesmente sai da lista — nao
/// ha arquivo para confundir com ele.
fn canonicalizar(targets: &[PathBuf]) -> Vec<PathBuf> {
    targets.iter().filter_map(|t| t.canonicalize().ok()).collect()
}

fn find_modules(root: &Path, source: &Path, targets: &[PathBuf]) -> Vec<PathBuf> {
    let alvos = canonicalizar(targets);
    let mut found = Vec::new();
    for entry in WalkDir::new(root.join(source)).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|e| e != "luau") {
            continue;
        }
        if is_generated(path, &alvos) {
            continue;
        }
        if path.components().any(|c| c.as_os_str() == "Modux") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        if has_call(&text) {
            found.push(path.to_path_buf());
        }
    }
    found.sort();
    found
}

fn has_call(text: &str) -> bool {
    for kind in ["Controller", "Service", "Component"] {
        let target = format!(".{kind}");
        let mut i = 0;
        while let Some(pos) = text[i..].find(&target) {
            let after = &text[i + pos + target.len()..];
            let remainder = after.trim_start();
            if remainder.starts_with('(') {
                let inside = remainder[1..].trim_start();
                if inside.starts_with('"') || inside.starts_with('\'') {
                    return true;
                }
            }
            i += pos + target.len();
        }
    }
    false
}

type Signature = (SystemTime, u64);

struct State {
    root: PathBuf,
    map: Map,
    layout: manifest::Layout,
    cache: BTreeMap<PathBuf, (Signature, Module)>,
}

impl State {
    fn new(root: &Path, source: &Source, layout: manifest::Layout) -> Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            map: source.read()?,
            layout,
            cache: BTreeMap::new(),
        })
    }

    fn signature(path: &Path) -> Option<Signature> {
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    }

    fn rel(&self, path: &Path) -> String {
        path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn extract(&mut self, path: &Path) -> Result<(Module, bool)> {
        let signature = Self::signature(path);
        if let (Some(a), Some((previous, data))) = (signature, self.cache.get(path)) {
            if *previous == a {
                return Ok((data.clone(), false));
            }
        }
        let mut data = Extractor::new(path)?.run()?;
        data.file = self.rel(path);
        if let Some(a) = signature {
            self.cache.insert(path.to_path_buf(), (a, data.clone()));
        }
        Ok((data, true))
    }

    fn modules(&mut self) -> Result<Vec<Module>> {
        let targets_list = find_modules(&self.root, &self.layout.source, &self.paths());
        let mut out = Vec::new();
        for target in targets_list {
            out.push(self.extract(&target)?.0);
        }
        Ok(out)
    }

    fn write_if_changed(path: &Path, text: &str) -> Result<bool> {
        if let Ok(previous) = std::fs::read_to_string(path) {
            if previous == text {
                return Ok(false);
            }
        }
        if let Some(pai) = path.parent() {
            std::fs::create_dir_all(pai)?;
        }
        std::fs::write(path, text)
            .with_context(|| format!("could not write {}", path.display()))?;
        Ok(true)
    }

    /// Recusa escrever uma folha que ocuparia o caminho de DataModel de outro
    /// arquivo.
    ///
    /// O rogen mapeia `src/<Feature>/<lado>` para `<Raiz>.<lado>.<Feature>`, e
    /// duas pastas de disco diferentes podem cair no MESMO caminho. Quando
    /// isso acontece ele desce para entradas por arquivo e mescla as duas — nao
    /// da erro. O resultado e pior que erro: se os dois lados tiverem um
    /// arquivo de mesmo nome, um simplesmente SOME do project file, e quem
    /// requer aquele caminho passa a receber o outro.
    ///
    /// Foi por isso que a pasta gerada deixou de se chamar `Types` (ver
    /// `Layout::types_dirs`), mas trocar o nome so resolve a colisao que eu
    /// conhecia. Esta guarda cobre as que eu nao conheco: qualquer arquivo do
    /// projeto que por acaso divida o caminho com uma folha.
    fn check_leaf_collisions(
        &self,
        modules: &[Module],
        sides: &std::collections::BTreeMap<String, rojo::Side>,
    ) -> Result<()> {
        let dirs: Vec<PathBuf> = self.layout.types_dirs.iter().map(|(_, p)| p.clone()).collect();
        let dentro = |p: &Path| dirs.iter().any(|d| p.starts_with(d));

        // O caminho de DataModel de CADA pasta de folha, deduzido de uma folha
        // que ja esteja no mapa. Nao da para perguntar pela pasta direto: o
        // `Map` casa por prefixo contra o project file, e o rogen so registra
        // caminho de arquivo quando duas pastas se fundem.
        let mut raizes: Vec<(String, PathBuf)> = Vec::new();
        for m in modules {
            let leaf = emit::leaf_path(&self.layout, sides[&m.id], &m.id);
            let Ok(dm) = self.map.path(&leaf) else { continue };
            let Some(pai) = dm.rsplit_once('.').map(|(a, _)| a.to_string()) else { continue };
            let Some(dir) = leaf.parent().map(Path::to_path_buf) else { continue };
            if !raizes.iter().any(|(p, _)| *p == pai) {
                raizes.push((pai, dir));
            }
        }

        // Um arquivo QUE NAO E FOLHA caindo debaixo de uma dessas raizes
        // significa que duas pastas de disco viraram a mesma instancia.
        //
        // Reparar no arquivo perdido nao funcionaria: quando duas pastas se
        // fundem e ha nome repetido, o perdedor SOME do project file, entao
        // `map.path` nem responde por ele. Quem denuncia a fusao sao os
        // VIZINHOS dele, que continuam mapeados e agora dividem a raiz com as
        // folhas.
        for entry in WalkDir::new(self.root.join(&self.layout.source))
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            let path = entry.path();
            if !path.is_file() || path.extension().is_none_or(|e| e != "luau") {
                continue;
            }
            // Relativizar ANTES de qualquer comparacao. WalkDir entrega
            // caminho absoluto; `layout.types_dirs` e as entradas do project
            // file sao relativas a raiz. Comparar absoluto com relativo nao da
            // erro, so nunca casa — custou um falso negativo na guarda e um
            // falso positivo na exclusao, cada um numa rodada diferente.
            let relativo = path.strip_prefix(&self.root).unwrap_or(path);
            if dentro(relativo) {
                continue;
            }
            let Ok(dm) = self.map.path(relativo) else { continue };
            for (raiz, dir) in &raizes {
                if dm.starts_with(&format!("{raiz}.")) {
                    bail!(
                        "{} e {} viram a MESMA instancia ({}).\n  \
                         Quando duas pastas se fundem e ha nome repetido, uma some \
                         do project file sem aviso.\n  \
                         Renomeie a pasta do projeto, ou o modulo.",
                        self.rel(dir),
                        self.rel(path),
                        raiz
                    );
                }
            }
        }
        Ok(())
    }

    /// Remove de `ModuxTypes/` toda folha sem modulo correspondente. Devolve se
    /// apagou alguma coisa.
    fn sweep_type_dirs(
        layout: &manifest::Layout,
        modules: &[Module],
        sides: &std::collections::BTreeMap<String, rojo::Side>,
    ) -> Result<bool> {
        let esperadas: std::collections::BTreeSet<PathBuf> = modules
            .iter()
            .map(|m| emit::leaf_path(layout, sides[&m.id], &m.id))
            .collect();

        let mut apagou = false;
        for (_, dir) in &layout.types_dirs {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "luau") {
                    continue;
                }
                if esperadas.contains(&path) {
                    continue;
                }
                std::fs::remove_file(&path)
                    .with_context(|| format!("could not remove {}", path.display()))?;
                log(&format!(
                    "stale leaf removed: {}",
                    path.display().to_string().replace('\\', "/")
                ));
                apagou = true;
            }
        }
        Ok(apagou)
    }

    fn pass(&mut self, verbose: bool) -> Result<bool> {
        self.ensure_type_dirs();
        let targets_list = find_modules(&self.root, &self.layout.source, &self.paths());
        let seen: Vec<PathBuf> = targets_list.clone();
        self.cache.retain(|k, _| seen.contains(k));

        let mut modules = Vec::new();
        let mut changed = false;
        let mut pending_leaves: Vec<(PathBuf, Module)> = Vec::new();

        for target in &targets_list {
            let (data, is_new) = self.extract(target)?;
            if is_new || verbose {
                for p in &data.issues {
                    eprintln!(
                        "[modux] {}:{}:{} {}",
                        data.file, p.line, p.column, p.message
                    );
                }
            }
            if is_new {
                pending_leaves.push((target.clone(), data.clone()));
            }
            modules.push(data);
        }

        let sides = manifest::validate(&modules, &self.map)?;

        self.check_leaf_collisions(&modules, &sides)?;

        for (_, data) in &pending_leaves {
            let leaf = emit::leaf_path(&self.layout, sides[&data.id], &data.id);
            // O caminho de DataModel do MODULO (nao da folha): e contra ele que
            // os requires copiados sao absolutizados, ver emit::rewrite_require.
            let origem = self.map.path(Path::new(&data.file))?;
            if Self::write_if_changed(&leaf, &emit::emit(data, &origem))? {
                log(&format!("leaf: {}", self.rel(&leaf)));
                changed = true;
            }
        }

        // Folha anterior a 0.7.0 ainda ao lado do modulo (ver legacy_leaves):
        // aqui so avisa, nunca apaga. Remover arquivo fora de `Types/` e ato de
        // migracao, e migracao se pede — `modux fix`.
        let legadas = targets_list
            .iter()
            .filter(|t| t.parent().is_some_and(|p| p.join("Type.luau").exists()))
            .count();
        if legadas > 0 {
            log(&format!(
                "{legadas} folha(s) anterior(es) a 0.7.0 ao lado do modulo, com uma copia desatualizada do tipo; `modux fix` remove"
            ));
        }

        // Folha de modulo que nao existe mais.
        //
        // Enquanto a folha morava ao lado do modulo, isto era de graca: apagar
        // a pasta levava a folha junto. Centralizada, ninguem a apaga, e uma
        // folha obsoleta e pior que um arquivo a toa — ela continua sendo um
        // ModuleScript valido no DataModel, e o LSP segue oferecendo um tipo de
        // um modulo que ja morreu, sem nenhum erro em lugar nenhum. E a mesma
        // familia de bug de entrada obsoleta que custou caro no SyncTeam.
        if Self::sweep_type_dirs(&self.layout, &modules, &sides)? {
            changed = true;
        }

        for (side, target) in self.layout.targets.clone() {
            let Some(text) = manifest::emit(side, &modules, &sides, &self.map, &self.layout)? else {
                continue;
            };
            if Self::write_if_changed(&target, &text)? {
                let n = modules.iter().filter(|m| side.sees(sides[&m.id])).count();
                log(&format!("manifest {}: {} ({n} modules)", side.name(), self.rel(&target)));
                changed = true;
            }
        }

        for (side, target) in self.layout.module_lists.clone() {
            let Some(text) = manifest::emit_module_list(side, &modules, &sides, &self.map)? else {
                continue;
            };
            if Self::write_if_changed(&target, &text)? {
                log(&format!("modules {}: {}", side.name(), self.rel(&target)));
                changed = true;
            }
        }

        let libs = self.layout.libs_target.clone();
        let text = manifest::emit_libs(&self.root, &self.layout.libs_dir, &self.map)?;
        if Self::write_if_changed(&libs, &text)? {
            log(&format!("libs: {}", self.rel(&libs)));
            changed = true;
        }

        Ok(changed)
    }

    fn paths(&self) -> Vec<PathBuf> {
        let mut all: Vec<PathBuf> = self.layout.targets.iter().map(|(_, p)| p.clone()).collect();
        all.extend(self.layout.module_lists.iter().map(|(_, p)| p.clone()));
        // As pastas de tipo inteiras, nao arquivos: tudo la dentro e gerado, e
        // `is_generated` testa ancestralidade. Sem isto o proprio gerador
        // encontraria as folhas que acabou de escrever e tentaria trata-las
        // como modulos.
        all.extend(self.layout.types_dirs.iter().map(|(_, p)| p.clone()));
        all.push(self.layout.libs_target.clone());
        all
    }

    /// Cria as pastas de tipo mesmo vazias.
    ///
    /// O rogen deriva o project file da estrutura de pastas, e o
    /// `Map::path` resolve a folha casando o PREFIXO do caminho contra as
    /// entradas do mapa. Uma pasta que nao existe no disco nao entra no
    /// project file, e ai a folha nao teria endereco de DataModel nenhum.
    /// Criar cedo, antes de qualquer escrita, e o que quebra esse
    /// galinha-e-ovo entre as duas ferramentas.
    fn ensure_type_dirs(&self) {
        for (_, dir) in &self.layout.types_dirs {
            let _ = std::fs::create_dir_all(dir);
        }
    }

    fn check_stale(&mut self) -> Result<Vec<PathBuf>> {
        let modules = self.modules()?;
        let mut stale = Vec::new();

        let sides = manifest::validate(&modules, &self.map)?;

        for m in &modules {
            let leaf = emit::leaf_path(&self.layout, sides[&m.id], &m.id);
            let expected = emit::emit(m, &self.map.path(Path::new(&m.file))?);
            if std::fs::read_to_string(&leaf).ok().as_deref() != Some(expected.as_str()) {
                stale.push(leaf);
            }
        }
        for (side, target) in self.layout.targets.clone() {
            let Some(text) = manifest::emit(side, &modules, &sides, &self.map, &self.layout)? else {
                continue;
            };
            if std::fs::read_to_string(&target).ok().as_deref() != Some(text.as_str()) {
                stale.push(target);
            }
        }
        for (side, target) in self.layout.module_lists.clone() {
            let Some(text) = manifest::emit_module_list(side, &modules, &sides, &self.map)? else {
                continue;
            };
            if std::fs::read_to_string(&target).ok().as_deref() != Some(text.as_str()) {
                stale.push(target);
            }
        }

        let libs = self.layout.libs_target.clone();
        let text = manifest::emit_libs(&self.root, &self.layout.libs_dir, &self.map)?;
        if std::fs::read_to_string(&libs).ok().as_deref() != Some(text.as_str()) {
            stale.push(libs);
        }

        Ok(stale)
    }
}

fn snapshot(root: &Path, source: &Path, targets: &[PathBuf]) -> BTreeMap<PathBuf, Signature> {
    let alvos = canonicalizar(targets);
    let mut state = BTreeMap::new();
    for entry in WalkDir::new(root.join(source)).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|e| e != "luau") {
            continue;
        }
        if is_generated(path, &alvos) {
            continue;
        }
        if let Some(a) = State::signature(path) {
            state.insert(path.to_path_buf(), a);
        }
    }
    state
}

/// Regera o sourcemap do zero, chamando o rojo.
///
/// Duas coisas obrigam a isto, e nenhuma delas se resolve sozinha:
///
/// O `rojo sourcemap --watch` le o default.project.json uma vez, na partida, e
/// nunca mais. Renomear uma pasta muda a arvore, o rogen reescreve o project
/// file, e o watch do rojo segue observando a arvore velha: o sourcemap
/// congela e o language server passa a resolver caminhos que nao existem mais.
/// Medido num projeto real, com o sourcemap vinte minutos atras do disco.
///
/// Pior, o mesmo watch MORRE quando uma pasta observada e apagada. No Rojo
/// 7.7.0: `called Result::unwrap() on an Err value: ... Canonicalize`, em
/// change_processor.rs:179. Dali em diante o sourcemap.json e um arquivo
/// parado: nenhuma edicao seguinte chega nele. Um processo morto nao volta
/// sozinho, entao quem repara e este rebuild.
///
/// Falhar aqui nao e motivo para derrubar nada: sem o rojo no PATH, o estado
/// volta a ser o de antes.
fn rebuild_sourcemap(root: &Path) -> bool {
    std::process::Command::new("rojo")
        .current_dir(root)
        .args(["sourcemap", PROJECT, "-o", "sourcemap.json"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Reescreve o sourcemap com o proprio conteudo.
///
/// Mudar so o CORPO de um Type.luau nao altera a arvore do projeto, entao o
/// `rojo sourcemap --watch` nao reescreve nada — medido — e o language server
/// fica sem o aviso que ele de fato escuta. O arquivo tem alguns KB, e um write
/// e o evento mais barato que o alcanca. Nao ha risco de laco: o rojo observa o
/// project file e as fontes, nunca o sourcemap que ele mesmo emite.
fn nudge_sourcemap(root: &Path) {
    let path = root.join("sourcemap.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return };

    // Escrita atomica, porque o `rojo sourcemap --watch` pode estar escrevendo
    // o mesmo arquivo: um write direto deixaria uma janela em que o server le
    // JSON pela metade. Com rename no mesmo volume, quem le ve a versao velha
    // ou a nova, nunca um meio-termo.
    let temporary = path.with_extension("json.nudge");
    if std::fs::write(&temporary, text).is_ok() && std::fs::rename(&temporary, &path).is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

fn watch_loop(
    state: &mut State,
    source: &Source,
    interval: Duration,
    autofix_on: bool,
    nudge: bool,
) -> Result<()> {
    log(&format!("watching {}/ (Ctrl+C to stop)", state.layout.source.display()));
    let mut previous = snapshot(&state.root, &state.layout.source, &state.paths());
    let origem = source.file().to_path_buf();
    let mut origem_at = State::signature(&origem);

    loop {
        std::thread::sleep(interval);

        // O mapa do Rojo era lido uma vez so, na partida. Com o rogen rodando
        // ao lado, uma feature nova reescreve o project file e o modux seguia
        // com o mapa velho ate ser reiniciado, errando `path outside
        // default.project.json` para sempre. E o snapshot nao ajudava: ele
        // varre src/, e o project file mora na raiz.
        let now = State::signature(&origem);
        let mut forced = false;
        if now != origem_at {
            origem_at = now;
            match source.read() {
                Ok(map) => {
                    state.map = map;
                    state.cache.clear();
                    log(&format!("{} changed, reloaded", state.rel(&origem)));
                    forced = true;
                }
                Err(err) => log(&format!("ERROR: could not reload {}: {err:#}", state.rel(&origem))),
            }
        }

        let current = snapshot(&state.root, &state.layout.source, &state.paths());
        if current == previous && !forced {
            continue;
        }

        let paths: std::collections::BTreeSet<&PathBuf> =
            current.keys().chain(previous.keys()).collect();
        // Editar o corpo de um arquivo nao mexe na arvore; um arquivo que nasce
        // ou some, sim. So o segundo caso obriga a refazer o sourcemap, e a
        // diferenca importa porque refazer custa uns 400 ms.
        let mut tree_moved = false;
        for path in paths {
            let a = previous.get(path);
            let b = current.get(path);
            if a == b {
                continue;
            }
            let label = match (a, b) {
                (None, _) => {
                    tree_moved = true;
                    "new"
                }
                (_, None) => {
                    tree_moved = true;
                    "deleted"
                }
                _ => "changed",
            };
            log(&format!("{label}: {}", state.rel(path)));
        }
        previous = current;

        // Antes da pass: mover primeiro deixa a geracao ja escrever a folha no
        // lugar novo, em vez de escrever no pai e apagar em seguida.
        let just_moved = autofix_on && autofix(state);
        if just_moved {
            previous = snapshot(&state.root, &state.layout.source, &state.paths());
            tree_moved = true;
        }

        let started = Instant::now();
        let mut wrote = false;
        match state.pass(false) {
            Ok(changed) => {
                wrote = changed;
                log(&format!("regenerated in {} ms", started.elapsed().as_millis()))
            }
            // Uma pasta que acabou de nascer ainda nao esta no project file, e
            // so estara quando o rogen correr. Enquanto isso a geracao nao tem
            // como resolver o caminho — e espera, nao falha, entao nao se
            // anuncia como erro.
            Err(err) if just_moved && format!("{err:#}").contains("outside") => {
                log("waiting for rogen to pick up the new folder")
            }
            // Sintaxe incompleta enquanto se digita nao e falha do projeto: o
            // leaf anterior continua no lugar e a proxima pass resolve. Uma
            // linha basta; o dump do parser so afogaria o log.
            Err(err) => match err.downcast_ref::<ast::Syntax>() {
                Some(broken) => {
                    let at = broken.line.map(|n| format!(":{n}")).unwrap_or_default();
                    log(&format!("incomplete: {}{at}", state.rel(&broken.file)))
                }
                None => log(&format!("ERROR: {err:#}")),
            },
        }

        // Depois da pass, de proposito: a pass acabou de escrever as folhas, e
        // uma folha nova e um no a mais na arvore. Refazer antes deixaria o
        // sourcemap sem ela ate a proxima volta.
        //
        // Refazer quando a arvore muda nao e luxo, e o unico jeito de o mapa
        // sobreviver a apagar uma pasta. Medido no Rojo 7.7.0: apagar uma pasta
        // observada MATA o `rojo sourcemap --watch`, com
        // `called Result::unwrap() on an Err value: Canonicalize` em
        // change_processor.rs:179. O processo morre, o sourcemap.json congela
        // no ultimo estado, e dali em diante nada mais entra nele — modulo novo
        // nao aparece, apagado nao sai. E o "so volta se eu rodar o analyzer",
        // porque o analyzer gera um sourcemap avulso, que nasce correto.
        //
        // Reagir so a mudanca do project file nao cobria: o rogen mapeia cada
        // pasta de lado de feature como `$path`, entao mexer num modulo dentro
        // de uma feature que ja existe nao reescreve o project file.
        if nudge {
            // Refazer so quando o sourcemap e derivado do project file. Tocar,
            // sempre: o toque reescreve os mesmos bytes, entao nao atropela
            // mapa de ninguem, e e o unico aviso que o language server escuta
            // quando so o CORPO de uma folha mudou.
            if (forced || tree_moved) && source.owns_sourcemap() {
                if rebuild_sourcemap(&state.root) {
                    log("sourcemap rebuilt");
                }
            } else if wrote || forced || tree_moved {
                nudge_sourcemap(&state.root);
            }
        }
    }
}

fn log(msg: &str) {
    println!("[modux] {msg}");
}
