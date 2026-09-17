//! CLI do gerador de tipos do Modux.
//!
//! Voce escreve o corpo do modulo; o modux escreve a folha de tipos e o
//! Manifest. Nao ha inferencia: ele le a anotacao que voce escreveu e alarga
//! literal. Se o modux sumir, o codigo continua Luau valido que roda.

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

use extract::{Extrator, Modulo};
use rojo::Mapa;

const PROJETO: &str = "default.project.json";
const FONTE: &str = "src";
const INTERVALO_PADRAO_MS: u64 = 400;

#[derive(Parser)]
#[command(
    name = "modux",
    version,
    about = "Gerador de tipos do framework Modux para Roblox",
    long_about = "Le os modulos do projeto com o luau-ast e escreve as folhas de \
                  tipo (Type.luau) e o Manifest.\n\n\
                  Precisa do `luau-ast` no PATH:  rokit add luau-lang/luau"
)]
struct Cli {
    /// Raiz do projeto. Por padrao procura o default.project.json subindo a partir do diretorio atual.
    #[arg(long, short, global = true, value_name = "CAMINHO")]
    projeto: Option<PathBuf>,

    #[command(subcommand)]
    comando: Comando,
}

#[derive(Subcommand)]
enum Comando {
    /// Regera tudo uma vez e sai.
    Generate,

    /// Observa `src/` e regera o que mudar.
    Watch {
        /// Intervalo entre checagens, em milissegundos.
        #[arg(long, default_value_t = INTERVALO_PADRAO_MS)]
        intervalo: u64,
    },

    /// Verifica se o que esta no disco bate com o que seria gerado. Nao escreve.
    /// Sai com codigo 1 quando algo esta desatualizado — serve para CI e pre-commit.
    Check,

    /// Lista os modulos encontrados e suas dependencias.
    List,

    /// Despeja o que o extrator ve num modulo, em JSON. Para depurar.
    Extract {
        /// Arquivo do modulo (o `init.luau`).
        arquivo: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(erro) = rodar(&cli) {
        eprintln!("modux: {erro:#}");
        std::process::exit(1);
    }
}

fn rodar(cli: &Cli) -> Result<()> {
    match &cli.comando {
        Comando::Extract { arquivo } => {
            let dados = Extrator::novo(arquivo)?.rodar()?;
            println!("{}", serde_json::to_string_pretty(&dados)?);
            Ok(())
        }
        Comando::Generate => {
            let raiz = raiz_do_projeto(cli)?;
            let mut estado = Estado::novo(&raiz)?;
            let inicio = Instant::now();
            let mudou = estado.passada(true)?;
            if !mudou {
                log("tudo em dia");
            }
            log(&format!("{} ms", inicio.elapsed().as_millis()));
            Ok(())
        }
        Comando::Check => {
            let raiz = raiz_do_projeto(cli)?;
            let mut estado = Estado::novo(&raiz)?;
            let pendentes = estado.conferir()?;
            if pendentes.is_empty() {
                log("tudo em dia");
                return Ok(());
            }
            for p in &pendentes {
                eprintln!("desatualizado: {}", p.display());
            }
            bail!(
                "{} arquivo(s) desatualizado(s). Rode `modux generate`.",
                pendentes.len()
            )
        }
        Comando::List => {
            let raiz = raiz_do_projeto(cli)?;
            let mut estado = Estado::novo(&raiz)?;
            for m in estado.modulos()? {
                let deps = if m.dependencias.is_empty() {
                    "-".to_string()
                } else {
                    m.dependencias.join(", ")
                };
                println!("{:<12} {:<10} {}  deps: {}", m.id, m.especie, m.arquivo, deps);
            }
            Ok(())
        }
        Comando::Watch { intervalo } => {
            let raiz = raiz_do_projeto(cli)?;
            let mut estado = Estado::novo(&raiz)?;
            let inicio = Instant::now();
            if !estado.passada(true)? {
                log("tudo em dia");
            }
            log(&format!("primeira passada em {} ms", inicio.elapsed().as_millis()));
            observar(&mut estado, Duration::from_millis(*intervalo))
        }
    }
}

// ---- projeto ---------------------------------------------------------------

fn raiz_do_projeto(cli: &Cli) -> Result<PathBuf> {
    if let Some(p) = &cli.projeto {
        if !p.join(PROJETO).exists() {
            bail!("{} nao tem {PROJETO}", p.display());
        }
        return Ok(p.clone());
    }
    let mut atual = std::env::current_dir().context("nao consegui ler o diretorio atual")?;
    loop {
        if atual.join(PROJETO).exists() {
            return Ok(atual);
        }
        if !atual.pop() {
            bail!(
                "nao achei {PROJETO} aqui nem nos diretorios acima.\n\
                 Rode dentro do projeto, ou passe --projeto CAMINHO."
            );
        }
    }
}

/// Folha e Manifest sao saida nossa: observar geraria rodada em falso.
fn eh_gerado(caminho: &Path, destino_manifest: &Path) -> bool {
    if caminho.file_name().is_some_and(|n| n == "Type.luau") {
        return true;
    }
    match (caminho.canonicalize(), destino_manifest.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Arquivos que chamam Controller, Service ou Component.
fn achar_modulos(raiz: &Path, destino_manifest: &Path) -> Vec<PathBuf> {
    let mut achados = Vec::new();
    for entrada in WalkDir::new(raiz.join(FONTE)).into_iter().filter_map(Result::ok) {
        let caminho = entrada.path();
        if !caminho.is_file() || caminho.extension().is_none_or(|e| e != "luau") {
            continue;
        }
        if eh_gerado(caminho, destino_manifest) {
            continue;
        }
        // O proprio framework nao e um modulo do usuario.
        if caminho.components().any(|c| c.as_os_str() == "Modux") {
            continue;
        }
        let Ok(texto) = std::fs::read_to_string(caminho) else { continue };
        if tem_chamada(&texto) {
            achados.push(caminho.to_path_buf());
        }
    }
    achados.sort();
    achados
}

/// Procura `.Controller(" / .Service(" / .Component("` sem precisar de regex.
fn tem_chamada(texto: &str) -> bool {
    for especie in ["Controller", "Service", "Component"] {
        let alvo = format!(".{especie}");
        let mut i = 0;
        while let Some(pos) = texto[i..].find(&alvo) {
            let depois = &texto[i + pos + alvo.len()..];
            let resto = depois.trim_start();
            if resto.starts_with('(') {
                let dentro = resto[1..].trim_start();
                if dentro.starts_with('"') || dentro.starts_with('\'') {
                    return true;
                }
            }
            i += pos + alvo.len();
        }
    }
    false
}

// ---- estado ----------------------------------------------------------------

type Assinatura = (SystemTime, u64);

struct Estado {
    raiz: PathBuf,
    mapa: Mapa,
    destino: PathBuf,
    /// caminho -> (assinatura, dados). Reextrai so o que mudou de verdade.
    cache: BTreeMap<PathBuf, (Assinatura, Modulo)>,
}

impl Estado {
    fn novo(raiz: &Path) -> Result<Self> {
        Ok(Self {
            raiz: raiz.to_path_buf(),
            mapa: Mapa::ler(&raiz.join(PROJETO))?,
            destino: manifest::destino(raiz),
            cache: BTreeMap::new(),
        })
    }

    fn assinatura(caminho: &Path) -> Option<Assinatura> {
        let meta = std::fs::metadata(caminho).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    }

    fn rel(&self, caminho: &Path) -> String {
        caminho
            .strip_prefix(&self.raiz)
            .unwrap_or(caminho)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Reextrai so quando o arquivo mudou. Cada `luau-ast` custa dezenas de ms
    /// e o binario aceita um arquivo por invocacao, entao reextrair o projeto
    /// inteiro a cada save custaria N vezes isso.
    fn extrair(&mut self, caminho: &Path) -> Result<(Modulo, bool)> {
        let assin = Self::assinatura(caminho);
        if let (Some(a), Some((anterior, dados))) = (assin, self.cache.get(caminho)) {
            if *anterior == a {
                return Ok((dados.clone(), false));
            }
        }
        let mut dados = Extrator::novo(caminho)?.rodar()?;
        dados.arquivo = self.rel(caminho);
        if let Some(a) = assin {
            self.cache.insert(caminho.to_path_buf(), (a, dados.clone()));
        }
        Ok((dados, true))
    }

    fn modulos(&mut self) -> Result<Vec<Modulo>> {
        let alvos = achar_modulos(&self.raiz, &self.destino);
        let mut saida = Vec::new();
        for alvo in alvos {
            saida.push(self.extrair(&alvo)?.0);
        }
        Ok(saida)
    }

    /// Escreve so quando o conteudo muda. O watcher depende disso para nao
    /// disparar em si mesmo.
    fn escrever(caminho: &Path, texto: &str) -> Result<bool> {
        if let Ok(anterior) = std::fs::read_to_string(caminho) {
            if anterior == texto {
                return Ok(false);
            }
        }
        if let Some(pai) = caminho.parent() {
            std::fs::create_dir_all(pai)?;
        }
        std::fs::write(caminho, texto)
            .with_context(|| format!("nao consegui escrever {}", caminho.display()))?;
        Ok(true)
    }

    fn passada(&mut self, verboso: bool) -> Result<bool> {
        let alvos = achar_modulos(&self.raiz, &self.destino);
        let vistos: Vec<PathBuf> = alvos.clone();
        self.cache.retain(|k, _| vistos.contains(k));

        let mut modulos = Vec::new();
        let mut mudou = false;

        for alvo in &alvos {
            let (dados, novo) = self.extrair(alvo)?;
            if novo || verboso {
                for p in &dados.problemas {
                    eprintln!(
                        "[modux] {}:{}:{} {}",
                        dados.arquivo, p.linha, p.coluna, p.mensagem
                    );
                }
            }
            if novo {
                let folha = emit::caminho_da_folha(alvo);
                if Self::escrever(&folha, &emit::emitir(&dados))? {
                    log(&format!("folha: {}", self.rel(&folha)));
                    mudou = true;
                }
            }
            modulos.push(dados);
        }

        if modulos.is_empty() {
            bail!("nenhum modulo Modux encontrado em {FONTE}/");
        }

        let texto = manifest::emitir(&modulos, &self.mapa)?;
        if Self::escrever(&self.destino, &texto)? {
            log(&format!(
                "manifest: {} ({} modulos)",
                self.rel(&self.destino),
                modulos.len()
            ));
            mudou = true;
        }

        Ok(mudou)
    }

    /// Como `passada`, mas sem escrever: devolve o que sairia diferente.
    fn conferir(&mut self) -> Result<Vec<PathBuf>> {
        let modulos = self.modulos()?;
        if modulos.is_empty() {
            bail!("nenhum modulo Modux encontrado em {FONTE}/");
        }
        let mut pendentes = Vec::new();

        for m in &modulos {
            let folha = emit::caminho_da_folha(&self.raiz.join(&m.arquivo));
            let esperado = emit::emitir(m);
            if std::fs::read_to_string(&folha).ok().as_deref() != Some(esperado.as_str()) {
                pendentes.push(folha);
            }
        }

        let texto = manifest::emitir(&modulos, &self.mapa)?;
        if std::fs::read_to_string(&self.destino).ok().as_deref() != Some(texto.as_str()) {
            pendentes.push(self.destino.clone());
        }
        Ok(pendentes)
    }
}

// ---- watch -----------------------------------------------------------------

fn instantaneo(raiz: &Path, destino: &Path) -> BTreeMap<PathBuf, Assinatura> {
    let mut estado = BTreeMap::new();
    for entrada in WalkDir::new(raiz.join(FONTE)).into_iter().filter_map(Result::ok) {
        let caminho = entrada.path();
        if !caminho.is_file() || caminho.extension().is_none_or(|e| e != "luau") {
            continue;
        }
        if eh_gerado(caminho, destino) {
            continue;
        }
        if let Some(a) = Estado::assinatura(caminho) {
            estado.insert(caminho.to_path_buf(), a);
        }
    }
    estado
}

fn observar(estado: &mut Estado, intervalo: Duration) -> Result<()> {
    log(&format!("observando {}/ (Ctrl+C para parar)", FONTE));
    let mut anterior = instantaneo(&estado.raiz, &estado.destino);

    loop {
        std::thread::sleep(intervalo);
        let atual = instantaneo(&estado.raiz, &estado.destino);
        if atual == anterior {
            continue;
        }

        // Uniao das duas chaves, sem repetir: um caminho presente nos dois
        // mapas sairia duas vezes no log.
        let caminhos: std::collections::BTreeSet<&PathBuf> =
            atual.keys().chain(anterior.keys()).collect();
        for caminho in caminhos {
            let a = anterior.get(caminho);
            let b = atual.get(caminho);
            if a == b {
                continue;
            }
            let rotulo = match (a, b) {
                (None, _) => "novo",
                (_, None) => "apagado",
                _ => "mudou",
            };
            log(&format!("{rotulo}: {}", estado.rel(caminho)));
        }
        anterior = atual;

        let inicio = Instant::now();
        // Erro durante o watch nao derruba o loop: voce corrige e ele segue.
        match estado.passada(false) {
            Ok(_) => log(&format!("regerado em {} ms", inicio.elapsed().as_millis())),
            Err(erro) => log(&format!("ERRO: {erro:#}")),
        }
    }
}

fn log(msg: &str) {
    println!("[modux] {msg}");
}
