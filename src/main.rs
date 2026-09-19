
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

fn run(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Extract { file } => {
            let data = Extractor::new(file)?.run()?;
            println!("{}", serde_json::to_string_pretty(&data)?);
            Ok(())
        }
        Command::Generate => {
            let root = project_root(cli)?;
            let mut state = State::new(&root)?;
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
            let mut state = State::new(&root)?;
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
            let mut state = State::new(&root)?;
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
            fix(&root, *dry_run, *open)
        }
        Command::Watch { interval, fix: autofix_on } => {
            let root = project_root(cli)?;
            let mut state = State::new(&root)?;
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
            watch_loop(&mut state, Duration::from_millis(*interval), *autofix_on)
        }
    }
}

fn project_root(cli: &Cli) -> Result<PathBuf> {
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
                 Rode inside do project, ou passe --project PATH."
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
    find_modules(root, &state.paths())
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n != "init.luau"))
        .collect()
}

fn fix(root: &Path, dry_run: bool, open: bool) -> Result<()> {
    let state = State::new(root)?;
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

fn find_modules(root: &Path, targets: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in WalkDir::new(root.join(SOURCE)).into_iter().filter_map(Result::ok) {
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
    targets: Vec<(crate::rojo::Side, PathBuf)>,
    module_lists: Vec<(crate::rojo::Side, PathBuf)>,
    cache: BTreeMap<PathBuf, (Signature, Module)>,
}

impl State {
    fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            map: Map::read(&root.join(PROJECT))?,
            targets: manifest::targets(root),
            module_lists: manifest::module_lists(root),
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
        let targets_list = find_modules(&self.root, &self.paths());
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
        let targets_list = find_modules(&self.root, &self.paths());
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

        for (side, target) in self.targets.clone() {
            let Some(text) = manifest::emit(side, &modules, &sides, &self.map)? else {
                continue;
            };
            if Self::write_if_changed(&target, &text)? {
                let n = modules.iter().filter(|m| side.sees(sides[&m.id])).count();
                log(&format!("manifest {}: {} ({n} modules)", side.name(), self.rel(&target)));
                changed = true;
            }
        }

        for (side, target) in self.module_lists.clone() {
            let Some(text) = manifest::emit_module_list(side, &modules, &sides, &self.map)? else {
                continue;
            };
            if Self::write_if_changed(&target, &text)? {
                log(&format!("modules {}: {}", side.name(), self.rel(&target)));
                changed = true;
            }
        }

        let libs = self.root.join(manifest::LIBS_TARGET);
        let text = manifest::emit_libs(&self.root, &self.map)?;
        if Self::write_if_changed(&libs, &text)? {
            log(&format!("libs: {}", self.rel(&libs)));
            changed = true;
        }

        Ok(changed)
    }

    fn paths(&self) -> Vec<PathBuf> {
        let mut all: Vec<PathBuf> = self.targets.iter().map(|(_, p)| p.clone()).collect();
        all.extend(self.module_lists.iter().map(|(_, p)| p.clone()));
        all.push(self.root.join(manifest::LIBS_TARGET));
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
        for (side, target) in self.targets.clone() {
            let Some(text) = manifest::emit(side, &modules, &sides, &self.map)? else {
                continue;
            };
            if std::fs::read_to_string(&target).ok().as_deref() != Some(text.as_str()) {
                stale.push(target);
            }
        }
        for (side, target) in self.module_lists.clone() {
            let Some(text) = manifest::emit_module_list(side, &modules, &sides, &self.map)? else {
                continue;
            };
            if std::fs::read_to_string(&target).ok().as_deref() != Some(text.as_str()) {
                stale.push(target);
            }
        }

        let libs = self.root.join(manifest::LIBS_TARGET);
        let text = manifest::emit_libs(&self.root, &self.map)?;
        if std::fs::read_to_string(&libs).ok().as_deref() != Some(text.as_str()) {
            stale.push(libs);
        }

        Ok(stale)
    }
}

fn snapshot(root: &Path, targets: &[PathBuf]) -> BTreeMap<PathBuf, Signature> {
    let mut state = BTreeMap::new();
    for entry in WalkDir::new(root.join(SOURCE)).into_iter().filter_map(Result::ok) {
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

fn watch_loop(state: &mut State, interval: Duration, autofix_on: bool) -> Result<()> {
    log(&format!("watching {}/ (Ctrl+C to stop)", SOURCE));
    let mut previous = snapshot(&state.root, &state.paths());
    let project = state.root.join(PROJECT);
    let mut project_at = State::signature(&project);

    loop {
        std::thread::sleep(interval);

        // O mapa do Rojo era lido uma vez so, na partida. Com o rogen rodando
        // ao lado, uma feature nova reescreve o project file e o modux seguia
        // com o mapa velho ate ser reiniciado, errando `path outside
        // default.project.json` para sempre. E o snapshot nao ajudava: ele
        // varre src/, e o project file mora na raiz.
        let now = State::signature(&project);
        let mut forced = false;
        if now != project_at {
            project_at = now;
            match Map::read(&project) {
                Ok(map) => {
                    state.map = map;
                    state.cache.clear();
                    log("project file changed, reloaded");
                    forced = true;
                }
                Err(err) => log(&format!("ERROR: could not reload the project file: {err:#}")),
            }
        }

        let current = snapshot(&state.root, &state.paths());
        if current == previous && !forced {
            continue;
        }

        let paths: std::collections::BTreeSet<&PathBuf> =
            current.keys().chain(previous.keys()).collect();
        for path in paths {
            let a = previous.get(path);
            let b = current.get(path);
            if a == b {
                continue;
            }
            let label = match (a, b) {
                (None, _) => "new",
                (_, None) => "deleted",
                _ => "changed",
            };
            log(&format!("{label}: {}", state.rel(path)));
        }
        previous = current;

        // Antes da pass: mover primeiro deixa a geracao ja escrever a folha no
        // lugar novo, em vez de escrever no pai e apagar em seguida.
        let just_moved = autofix_on && autofix(state);
        if just_moved {
            previous = snapshot(&state.root, &state.paths());
        }

        let started = Instant::now();
        match state.pass(false) {
            Ok(_) => log(&format!("regenerated in {} ms", started.elapsed().as_millis())),
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
    }
}

fn log(msg: &str) {
    println!("[modux] {msg}");
}
