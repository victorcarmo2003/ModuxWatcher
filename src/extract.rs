//! Le a AST de um modulo e devolve tudo que o gerador precisa.
//!
//! Nao ha inferencia aqui. O extrator le a anotacao que voce escreveu e alarga
//! literal; quem sabe de tipo e voce ou o compilador, nunca ele.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::Value;

use crate::ast::{self, caminha, eh_local, indexa, lista, texto, tipo, Fonte};

#[derive(Debug, Clone, Serialize)]
pub struct Problema {
    pub linha: usize,
    pub coluna: usize,
    pub mensagem: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Metodo {
    pub nome: String,
    pub assinatura: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Campo {
    pub nome: String,
    pub tipo: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Import {
    pub alias: String,
    pub expressao: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Servico {
    pub alias: String,
    pub servico: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TipoLocal {
    pub nome: String,
    pub texto: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Modulo {
    pub id: String,
    /// `Controller`, `Service` ou `Component`.
    pub especie: String,
    pub arquivo: String,
    pub servicos: Vec<Servico>,
    pub requires: Vec<Import>,
    pub tipos_locais: Vec<TipoLocal>,
    pub metodos: Vec<Metodo>,
    pub campos: Vec<Campo>,
    /// Sai do USO (`self.Dependencies.X`), nao do `Require`.
    pub dependencias: Vec<String>,
    /// O `Require` escrito a mao, que vale como override da ordem de load.
    pub require_declarado: Vec<String>,
    pub problemas: Vec<Problema>,
}

/// Literal alarga de proposito: transcrito cru viraria singleton, e a segunda
/// atribuicao ao mesmo campo quebraria com `Expected this to be '"idle"'`.
fn literal(t: &str) -> Option<&'static str> {
    match t {
        "AstExprConstantString" => Some("string"),
        "AstExprConstantNumber" => Some("number"),
        "AstExprConstantBool" => Some("boolean"),
        _ => None,
    }
}

pub struct Extrator {
    caminho: PathBuf,
    fonte: Fonte,
    raiz: Vec<Value>,
    problemas: Vec<Problema>,
}

impl Extrator {
    pub fn novo(caminho: &Path) -> Result<Self> {
        let (raiz, fonte) = ast::analisar(caminho)?;
        Ok(Self {
            caminho: caminho.to_path_buf(),
            fonte,
            raiz,
            problemas: Vec::new(),
        })
    }

    fn texto_tipo(&self, anotacao: &Value) -> String {
        self.fonte.fatia(texto(anotacao, "location")).trim().to_string()
    }

    fn problema(&mut self, no: &Value, msg: String) {
        let loc = texto(no, "location");
        let inicio = loc.split('-').next().unwrap_or("0,0").trim();
        let mut partes = inicio.split(',');
        let linha: usize = partes.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let coluna: usize = partes.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        self.problemas.push(Problema {
            linha: linha + 1,
            coluna: coluna + 1,
            mensagem: msg,
        });
    }

    /// Tipo de uma expressao atribuida. `None` quando nao da para saber.
    fn tipo_do_valor(&mut self, valor: &Value, onde: &str) -> Option<String> {
        let t = tipo(valor);
        if t == "AstExprTypeAssertion" {
            return valor.get("annotation").map(|a| self.texto_tipo(a));
        }
        if let Some(prim) = literal(t) {
            return Some(prim.to_string());
        }
        if t == "AstExprFunction" {
            return Some(self.assinatura(valor, false));
        }
        self.problema(
            valor,
            format!("{onde} nao tem tipo. Anote com `::` para o gerador transcrever."),
        );
        None
    }

    fn assinatura(&mut self, func: &Value, com_self: bool) -> String {
        let todos = lista(func, "args").to_vec();
        let mut partes = Vec::new();

        let args: Vec<Value> = if com_self {
            partes.push("self: Public".to_string());
            // Duas formas chegam aqui e so uma traz `self` na lista de args:
            //   `function X:M(a)`       -> func.self preenchido, args = [a]
            //   `function X.M(self, a)` -> func.self nulo,       args = [self, a]
            // O `self` do usuario e sempre trocado pelo tipo da folha, que e
            // quem manda. Sem isso ele sairia duplicado na assinatura.
            let tem_self_implicito = func.get("self").map_or(true, Value::is_null);
            let primeiro_e_self = todos.first().map(|a| texto(a, "name")) == Some("self");
            if tem_self_implicito && primeiro_e_self {
                todos[1..].to_vec()
            } else {
                todos
            }
        } else {
            todos
        };

        for arg in &args {
            let nome = texto(arg, "name").to_string();
            match arg.get("luauType").filter(|v| !v.is_null()) {
                Some(anot) => partes.push(format!("{nome}: {}", self.texto_tipo(anot))),
                None => {
                    self.problema(func, format!("parametro `{nome}` sem anotacao de tipo."));
                    partes.push(format!("{nome}: unknown"));
                }
            }
        }

        format!("({}) -> {}", partes.join(", "), self.retorno(func))
    }

    /// O retorno chega em tres formas de no (`AstTypePackExplicit`,
    /// `AstTypePackVariadic`, generico). Fatiar cobre as tres de uma vez e
    /// preserva `(boolean, string)` e `...number` como escritos.
    fn retorno(&self, func: &Value) -> String {
        match func.get("returnAnnotation").filter(|v| !v.is_null()) {
            Some(ret) => self.texto_tipo(ret),
            None => "()".to_string(),
        }
    }

    pub fn rodar(mut self) -> Result<Modulo> {
        let Some((nome_local, id, require_declarado, especie)) = self.declaracao() else {
            bail!(
                "{}: nenhuma chamada a Controller, Service ou Component encontrada",
                self.caminho.display()
            );
        };

        let mut metodos = Vec::new();
        let mut campos: BTreeMap<String, String> = BTreeMap::new();
        let mut deps: BTreeSet<String> = BTreeSet::new();

        for st in self.raiz.clone() {
            let Some((nome, func)) = Self::corpo_de_metodo(&st, &nome_local) else {
                continue;
            };
            let assinatura = self.assinatura(&func, true);
            metodos.push(Metodo { nome, assinatura });
            self.varre_corpo(&func, &mut campos, &mut deps);
        }

        Ok(Modulo {
            id,
            especie,
            arquivo: self.caminho.to_string_lossy().replace('\\', "/"),
            servicos: self.servicos(),
            requires: self.requires(),
            tipos_locais: self.tipos_locais(),
            metodos,
            campos: campos
                .into_iter()
                .map(|(nome, tipo)| Campo { nome, tipo })
                .collect(),
            dependencias: deps.into_iter().collect(),
            require_declarado,
            problemas: self.problemas,
        })
    }

    /// Acha `const X = Classes.Controller("Id", props?)`.
    fn declaracao(&self) -> Option<(String, String, Vec<String>, String)> {
        for st in &self.raiz {
            if tipo(st) != "AstStatLocal" {
                continue;
            }
            let Some(chamada) = lista(st, "values").first() else {
                continue;
            };
            if tipo(chamada) != "AstExprCall" {
                continue;
            }
            let func = chamada.get("func")?;
            if tipo(func) != "AstExprIndexName" {
                continue;
            }
            let especie = texto(func, "index");
            if !matches!(especie, "Controller" | "Service" | "Component") {
                continue;
            }
            let args = lista(chamada, "args");
            let primeiro = args.first()?;
            if tipo(primeiro) != "AstExprConstantString" {
                continue;
            }
            let nome_local = lista(st, "vars").first().map(|v| texto(v, "name"))?.to_string();
            return Some((
                nome_local,
                texto(primeiro, "value").to_string(),
                Self::require_das_props(args),
                especie.to_string(),
            ));
        }
        None
    }

    fn require_das_props(args: &[Value]) -> Vec<String> {
        let Some(props) = args.get(1).filter(|p| tipo(p) == "AstExprTable") else {
            return Vec::new();
        };
        for item in lista(props, "items") {
            let chave = item.get("key").map(|k| texto(k, "value")).unwrap_or("");
            if chave != "Require" {
                continue;
            }
            let Some(v) = item.get("value") else { continue };
            if tipo(v) == "AstExprConstantString" {
                return vec![texto(v, "value").to_string()];
            }
            if tipo(v) == "AstExprTable" {
                return lista(v, "items")
                    .iter()
                    .filter_map(|i| i.get("value"))
                    .filter(|val| tipo(val) == "AstExprConstantString")
                    .map(|val| texto(val, "value").to_string())
                    .collect();
            }
        }
        Vec::new()
    }

    /// Aceita `function X:M()`, `function X.M(self)` e `X.M = function()`.
    fn corpo_de_metodo(st: &Value, alvo: &str) -> Option<(String, Value)> {
        if tipo(st) == "AstStatFunction" {
            let nome = st.get("name")?;
            if tipo(nome) == "AstExprIndexName" && eh_local(nome.get("expr")?, alvo) {
                return Some((texto(nome, "index").to_string(), st.get("func")?.clone()));
            }
        }
        if tipo(st) == "AstStatAssign" {
            let var = lista(st, "vars").first()?;
            let val = lista(st, "values").first()?;
            if tipo(var) == "AstExprIndexName"
                && eh_local(var.get("expr")?, alvo)
                && tipo(val) == "AstExprFunction"
            {
                return Some((texto(var, "index").to_string(), val.clone()));
            }
        }
        None
    }

    fn varre_corpo(
        &mut self,
        func: &Value,
        campos: &mut BTreeMap<String, String>,
        deps: &mut BTreeSet<String>,
    ) {
        // Coletar antes e resolver depois, porque `tipo_do_valor` precisa de
        // `&mut self` para registrar problema e o walker ja emprestou a arvore.
        let mut atribuicoes: Vec<(String, Value)> = Vec::new();
        if let Some(corpo) = func.get("body") {
            caminha(corpo, &mut |no: &Value| {
                if tipo(no) == "AstStatAssign" {
                    let vars = lista(no, "vars");
                    let vals = lista(no, "values");
                    for (var, val) in vars.iter().zip(vals.iter()) {
                        if tipo(var) != "AstExprIndexName" {
                            continue;
                        }
                        let Some(dentro) = var.get("expr") else { continue };
                        if !eh_local(dentro, "self") {
                            continue;
                        }
                        atribuicoes.push((texto(var, "index").to_string(), val.clone()));
                    }
                }
                // `self.Dependencies.Alguem` -> e daqui que sai o Manifest
                if tipo(no) == "AstExprIndexName" {
                    if let Some(dentro) = no.get("expr") {
                        if indexa(dentro, "Dependencies")
                            && dentro.get("expr").is_some_and(|e| eh_local(e, "self"))
                        {
                            deps.insert(texto(no, "index").to_string());
                        }
                    }
                }
            });
        }

        for (nome, valor) in atribuicoes {
            if campos.contains_key(&nome) {
                continue;
            }
            if let Some(t) = self.tipo_do_valor(&valor, &format!("self.{nome}")) {
                campos.insert(nome, t);
            }
        }
    }

    /// `local X = game:GetService("Y")`. A folha reemite os que aparecem na
    /// raiz de um require herdado.
    fn servicos(&self) -> Vec<Servico> {
        let mut saida = Vec::new();
        for st in &self.raiz {
            if tipo(st) != "AstStatLocal" {
                continue;
            }
            let Some(v) = lista(st, "values").first() else { continue };
            if tipo(v) != "AstExprCall" {
                continue;
            }
            let Some(func) = v.get("func") else { continue };
            if tipo(func) != "AstExprIndexName" || texto(func, "index") != "GetService" {
                continue;
            }
            let Some(arg) = lista(v, "args").first() else { continue };
            if tipo(arg) != "AstExprConstantString" {
                continue;
            }
            let Some(alias) = lista(st, "vars").first().map(|x| texto(x, "name")) else {
                continue;
            };
            saida.push(Servico {
                alias: alias.to_string(),
                servico: texto(arg, "value").to_string(),
            });
        }
        saida
    }

    fn requires(&self) -> Vec<Import> {
        let mut saida = Vec::new();
        for st in &self.raiz {
            if tipo(st) != "AstStatLocal" {
                continue;
            }
            let Some(v) = lista(st, "values").first() else { continue };
            if tipo(v) != "AstExprCall" {
                continue;
            }
            let Some(func) = v.get("func") else { continue };
            let eh_require = (tipo(func) == "AstExprGlobal" && texto(func, "global") == "require")
                || (tipo(func) == "AstExprLocal"
                    && func.get("local").map(|l| texto(l, "name")) == Some("require"));
            if !eh_require {
                continue;
            }
            let Some(arg) = lista(v, "args").first() else { continue };
            let Some(alias) = lista(st, "vars").first().map(|x| texto(x, "name")) else {
                continue;
            };
            saida.push(Import {
                alias: alias.to_string(),
                expressao: self.fonte.fatia(texto(arg, "location")).trim().to_string(),
            });
        }
        saida
    }

    /// Tipo declarado no corpo tem que ser COPIADO na folha: a folha nao pode
    /// requerer o corpo, senao fecha ciclo.
    fn tipos_locais(&self) -> Vec<TipoLocal> {
        self.raiz
            .iter()
            .filter(|st| tipo(st) == "AstStatTypeAlias")
            .map(|st| TipoLocal {
                nome: texto(st, "name").to_string(),
                texto: self.fonte.fatia(texto(st, "location")).trim().to_string(),
            })
            .collect()
    }
}
