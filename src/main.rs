
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

        /// Move a newly created loose module into its own folder and reopen it.
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

    /// Move every module that is a loose file into a folder of its own.
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
                let loose = loose_modules(&root, &state).len();
                if loose > 0 {
                    log(&format!(
                        "{loose} module(s) already loose; run `modux fix` for those.                          From here on, a new one is moved as it appears"
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

/// A module written as `Foo.luau` puts its type leaf in the parent folder, where
/// it collides with every other loose module beside it. Moving the code to
/// `Foo/init.luau` fixes that, and it is safe: Rojo turns a folder with an
/// init.luau into a ModuleScript of the folder's name, so `script` and
/// `script.Parent` keep pointing at exactly what they pointed at before. No
/// require has to be rewritten.
fn relocate(file: &Path, siblings: &[PathBuf]) -> Result<PathBuf> {
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .with_context(|| format!("{} has no usable name", file.display()))?;
    let folder = file.with_file_name(stem);
    let destination = folder.join("init.luau");

    if folder.exists() {
        bail!("cannot move {}: {} already exists", file.display(), folder.display());
    }

    std::fs::create_dir_all(&folder)
        .with_context(|| format!("could not create {}", folder.display()))?;
    std::fs::rename(file, &destination)
        .with_context(|| format!("could not move {}", file.display()))?;

    // A folha que ficou no pai foi escrita para este modulo, e agora pertence a
    // um caminho que nao existe mais. Sai junto, mas so se nenhum outro modulo
    // solto ainda a reivindique.
    let orphan = emit::leaf_path(file);
    let still_claimed = siblings
        .iter()
        .any(|other| other != file && emit::leaf_path(other) == orphan);
    if !still_claimed && orphan.exists() {
        let _ = std::fs::remove_file(&orphan);
    }

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

fn loose_modules(root: &Path, state: &State) -> Vec<PathBuf> {
    find_modules(root, &state.layout.source, &state.paths())
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n != "init.luau"))
        .collect()
}

fn fix(root: &Path, source: &Source, layout: manifest::Layout, dry_run: bool, open: bool) -> Result<()> {
    let state = State::new(root, source, layout)?;
    let loose = loose_modules(root, &state);

    if loose.is_empty() {
        log("every module already has its own folder");
        return Ok(());
    }

    if dry_run {
        for file in &loose {
            println!(
                "{}  ->  {}",
                state.rel(file),
                state.rel(&file.with_file_name(file.file_stem().unwrap_or_default()).join("init.luau"))
            );
        }
        return Ok(());
    }

    for file in &loose {
        let destination = relocate(file, &loose)?;
        log(&format!("{}  ->  {}", state.rel(file), state.rel(&destination)));
        if open {
            reopen(&destination);
        }
    }

    log("run `modux generate` to write the leaves in their new place");
    Ok(())
}

/// Um modulo que acabou de aparecer e ainda esta solto. Roda so dentro do
/// watch, e so depois da primeira pass: ali o cache ja conhece tudo que existia
/// antes, entao um caminho ausente dele e de fato novo — o arquivo que a pessoa
/// acabou de criar. Mover o projeto inteiro sem pedir seria outra coisa, e para
/// isso existe `modux fix`.
fn autofix(state: &mut State) -> bool {
    let root = state.root.clone();
    let loose = loose_modules(&root, state);
    let fresh: Vec<PathBuf> = loose
        .iter()
        .filter(|p| !state.cache.contains_key(*p))
        .cloned()
        .collect();

    let mut did = false;
    for file in &fresh {
        match relocate(file, &loose) {
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

fn is_generated(path: &Path, targets: &[PathBuf]) -> bool {
    if path.file_name().is_some_and(|n| n == "Type.luau") {
        return true;
    }
    let Ok(a) = path.canonicalize() else { return false };
    targets.iter().any(|d| d.canonicalize().is_ok_and(|b| a == b))
}

fn find_modules(root: &Path, source: &Path, targets: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in WalkDir::new(root.join(source)).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|e| e != "luau") {
            continue;
        }
        if is_generated(path, targets) {
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

    fn pass(&mut self, verbose: bool) -> Result<bool> {
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

        for (target, data) in &pending_leaves {
            let leaf = emit::leaf_path(target);
            if Self::write_if_changed(&leaf, &emit::emit(data))? {
                log(&format!("leaf: {}", self.rel(&leaf)));
                changed = true;
            }
        }

        for (side, target) in self.layout.targets.clone() {
            let Some(text) = manifest::emit(side, &modules, &sides, &self.map)? else {
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
        all.push(self.layout.libs_target.clone());
        all
    }

    fn check_stale(&mut self) -> Result<Vec<PathBuf>> {
        let modules = self.modules()?;
        let mut stale = Vec::new();

        let sides = manifest::validate(&modules, &self.map)?;

        for m in &modules {
            let leaf = emit::leaf_path(&self.root.join(&m.file));
            let expected = emit::emit(m);
            if std::fs::read_to_string(&leaf).ok().as_deref() != Some(expected.as_str()) {
                stale.push(leaf);
            }
        }
        for (side, target) in self.layout.targets.clone() {
            let Some(text) = manifest::emit(side, &modules, &sides, &self.map)? else {
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
    let mut state = BTreeMap::new();
    for entry in WalkDir::new(root.join(source)).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|e| e != "luau") {
            continue;
        }
        if is_generated(path, targets) {
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
