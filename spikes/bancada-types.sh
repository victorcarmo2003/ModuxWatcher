#!/usr/bin/env bash
# Bancada do layout de tipo centralizado (modux 0.7.0).
#
# A pergunta do usuario: mudamos SO onde o gerador cospe a folha, entao mover
# um modulo de lugar — inclusive de lado — deveria continuar levando o tipo
# junto. "Deveria" nao serve; aqui se mede.
#
# Cada cenario e uma operacao no disco seguida de `rogen build` + `modux
# generate`, e depois se pergunta ao disco o que sobrou. Copia limpa do
# ModuxTemplate por execucao: contaminacao de sandbox ja custou uma tabela
# inteira de resultados invalida noutra bancada.
set -u

TPL="D:/UserData/Documents/GitHub/ModuxTemplate"
M="D:/UserData/Documents/GitHub/modux/target/release/modux.exe"
SB="$(cd "$(dirname "$0")" && pwd)/bancada"

passou=0; falhou=0
ok()    { printf "  ok    %-52s %s\n" "$1" "${2:-}"; passou=$((passou+1)); }
falha() { printf "  FALHA %-52s %s\n" "$1" "${2:-}"; falhou=$((falhou+1)); }

ciclo() { ( cd "$SB" && rogen build >/dev/null 2>&1 && "$M" generate >/dev/null 2>&1 ); }

tem()   { [ -f "$SB/$1" ]; }
naotem() { [ ! -e "$SB/$1" ]; }

# Um require do Manifest apontando para o modulo dado.
manifesto_cita() { # manifesto_cita <lado> <Id>
	grep -q "Types\.$2)" "$SB/src/Modux/$1/Manifest/init.luau" 2>/dev/null
}

echo "== bancada de tipo centralizado =="
rm -rf "$SB"; mkdir -p "$SB"
( cd "$TPL" && cp -r src rokit.toml default.project.json wally.toml wally.lock \
	Packages ServerPackages DevPackages tools .luaurc "$SB/" 2>/dev/null )
( cd "$SB" && "$M" fix >/dev/null 2>&1 )
ciclo
echo "  base migrada: $(find "$SB/src/Types" -name '*.luau' | wc -l) folha(s)"
echo

# --- 1. modulo novo, solto, sem pasta --------------------------------------

echo "1. criar um Service SOLTO (o ponto da mudanca)"
mkdir -p "$SB/src/Novo/server"
cat > "$SB/src/Novo/server/EstoqueService.luau" <<'LUA'
--!strict
local ServerScriptService = game:GetService("ServerScriptService")
local Modux = require(ServerScriptService.server.Modux)

const EstoqueService = Modux.Service("EstoqueService")

function EstoqueService:Guardar(item: string, qtd: number)
	self.Total = qtd
end

return EstoqueService
LUA
ciclo
tem "src/Types/server/EstoqueService.luau" \
	&& ok "folha nasceu em Types/server" || falha "folha nao apareceu"
naotem "src/Novo/server/EstoqueService/Type.luau" \
	&& ok "nada foi escrito ao lado do modulo" || falha "escreveu Type.luau ao lado"
naotem "src/Novo/server/EstoqueService" \
	&& ok "o modulo continuou sendo um ARQUIVO" || falha "viraram pasta de novo"
manifesto_cita server EstoqueService \
	&& ok "Manifest server cita a folha" || falha "Manifest nao cita"
grep -q "Total: number" "$SB/src/Types/server/EstoqueService.luau" 2>/dev/null \
	&& ok "campo inferido chegou na folha" || falha "campo ausente na folha"

# --- 2. trocar de LADO: server -> client -----------------------------------

echo
echo "2. mover o modulo de server para client"
mkdir -p "$SB/src/Novo/client"
git -C "$SB" init -q 2>/dev/null
mv "$SB/src/Novo/server/EstoqueService.luau" "$SB/src/Novo/client/EstoqueService.luau"
ciclo
tem "src/Types/client/EstoqueService.luau" \
	&& ok "folha apareceu em Types/client" || falha "folha nao migrou de lado"
naotem "src/Types/server/EstoqueService.luau" \
	&& ok "a folha antiga foi coletada" || falha "folha orfa ficou em Types/server"
manifesto_cita client EstoqueService \
	&& ok "Manifest client passou a citar" || falha "Manifest client nao cita"
manifesto_cita server EstoqueService \
	&& falha "Manifest server AINDA cita" || ok "Manifest server parou de citar"

# --- 3. renomear -----------------------------------------------------------

echo
echo "3. renomear o modulo (arquivo e id)"
sed -i 's/EstoqueService/InventarioService/g' "$SB/src/Novo/client/EstoqueService.luau"
mv "$SB/src/Novo/client/EstoqueService.luau" "$SB/src/Novo/client/InventarioService.luau"
ciclo
tem "src/Types/client/InventarioService.luau" \
	&& ok "folha nova com o nome novo" || falha "folha nova ausente"
naotem "src/Types/client/EstoqueService.luau" \
	&& ok "folha do nome antigo coletada" || falha "folha antiga sobrou"

# --- 4. apagar -------------------------------------------------------------

echo
echo "4. apagar o modulo"
rm -f "$SB/src/Novo/client/InventarioService.luau"
ciclo
naotem "src/Types/client/InventarioService.luau" \
	&& ok "folha sumiu junto" || falha "folha orfa sobreviveu"

# --- 5. pasta com irmao NAO e achatada -------------------------------------

echo
echo "5. modulo com irmao continua pasta (fix nao pode achatar)"
tem "src/Profile/server/ProfileService/init.luau" \
	&& ok "ProfileService seguiu como pasta" || falha "achataram ProfileService"
tem "src/Profile/server/ProfileService/Template.luau" \
	&& ok "o irmao continua no lugar" || falha "o irmao sumiu"
grep -q "ProfileService\.Template" "$SB/src/Types/server/ProfileService.luau" 2>/dev/null \
	&& ok "require do irmao ficou ABSOLUTO na folha" || falha "require do irmao errado"

# --- 6. tudo junto ainda bate ----------------------------------------------

echo
echo "6. estado final"
( cd "$SB" && "$M" check >/dev/null 2>&1 ) \
	&& ok "modux check limpo" || falha "modux check acusou divergencia"

folhas=$(find "$SB/src/Types" -name '*.luau' | wc -l)
modulos=$( cd "$SB" && "$M" list 2>/dev/null | grep -c . )
[ "$folhas" = "$modulos" ] \
	&& ok "uma folha por modulo" "$folhas = $modulos" \
	|| falha "folha e modulo nao batem" "folhas=$folhas modulos=$modulos"

echo

# --- 7. a tipagem de verdade, nos dois lados -------------------------------
#
# Os cenarios acima olham o DISCO: arquivo certo, no lugar certo. Isso nao
# prova que o tipo RESOLVE — uma folha pode estar no lugar e o luau-lsp
# degradar tudo para `any` sem reclamar. Aqui o modulo de prova consome uma
# dependencia de verdade, e o analisador tem de aceitar.
#
# O criterio nao e "zero erro": o sandbox nao roda wally-package-types, entao
# os storybooks acusam DevPackages ausente de saida. O criterio e NENHUM erro
# tocar Types/ nem o modulo de prova.

echo
echo "7. o tipo resolve mesmo, nos dois lados"
prova() { # prova <lado> <Id> <corpo>
	rm -f "$SB/src/Novo/server"/*.luau "$SB/src/Novo/client"/*.luau 2>/dev/null
	mkdir -p "$SB/src/Novo/$1"
	printf '%s\n' "$3" > "$SB/src/Novo/$1/$2.luau"
	ciclo
	local saida
	saida=$(cd "$SB" && powershell -NoProfile -Command "& ./tools/analyze.ps1 -Detail" 2>&1)
	local total
	total=$(echo "$saida" | grep -o 'total: [^,]*' | head -1)
	if echo "$saida" | grep -qE "Types[\/]|$2"; then
		falha "$1: o analisador acusou a folha ou o modulo" "$total"
		echo "$saida" | grep -E "Types[\/]|$2" | head -2 | sed 's/^/        /'
	else
		ok "$1: nenhum erro toca Types/ nem o modulo" "$total"
	fi
}

prova server EstoqueService '--!strict
local ServerScriptService = game:GetService("ServerScriptService")
local Modux = require(ServerScriptService.server.Modux)

const EstoqueService = Modux.Service("EstoqueService", { Require = { "NetService" } })

function EstoqueService:Guardar(item: string, qtd: number)
	self.Total = qtd
	self.Ativo = self.Dependencies.NetService:IsRunning() :: boolean
end

return EstoqueService'

prova client EstoqueController '--!strict
local StarterPlayer = game:GetService("StarterPlayer")
local Modux = require(StarterPlayer.StarterPlayerScripts.client.Modux)

const EstoqueController = Modux.Controller("EstoqueController", { Require = { "NetController" } })

function EstoqueController:Guardar(item: string, qtd: number)
	self.Total = qtd
	self.Ativo = self.Dependencies.NetController.Running :: boolean
end

return EstoqueController'

echo "  Types/: $(find "$SB/src/Types" -name '*.luau' | sed "s|$SB/src/Types/||" | sort | tr '\n' ' ')"
echo
echo "== passou: $passou   falhou: $falhou =="
