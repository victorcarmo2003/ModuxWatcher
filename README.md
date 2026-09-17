# modux

Gerador de tipos do framework [Modux](https://github.com/victorcarmo2003/ModuxV3) para Roblox.

Você escreve o corpo do módulo. O `modux` escreve a folha de tipos (`Type.luau`) e o
Manifest. O `self` fica tipado, com autocomplete, sem você anotar nada.

**Não há inferência.** O gerador lê a anotação que você escreveu e alarga literal —
quem sabe de tipo é você ou o compilador, nunca ele. Se o `modux` sumir, o código
continua Luau válido que roda.

## Instalação

```sh
rokit add luau-lang/luau
rokit add victorcarmo2003/modux
```

O `luau-ast` vem no pacote `luau-lang/luau` e é **obrigatório** — é ele que parseia.
O mesmo pacote traz o `luau-analyze`, que você vai querer de qualquer jeito.

## Comandos

```sh
modux generate    # regera tudo uma vez e sai
modux watch       # observa src/ e regera o que mudar
modux check       # confere sem escrever; sai com 1 se algo está desatualizado
modux list        # lista os módulos e suas dependências
modux extract F   # despeja o que o extrator vê num módulo, em JSON
```

`--projeto CAMINHO` aponta para outra raiz. Sem ele, o `modux` procura o
`default.project.json` subindo a partir do diretório atual.

`modux check` serve para CI e pre-commit: falha quando alguém commitou o corpo
sem regerar a folha.

## O que ele lê

```lua
--!strict
local ReplicatedStorage = game:GetService("ReplicatedStorage")
local Classes = require(ReplicatedStorage.shared.Modux.Classes)
local FSM = require(ReplicatedStorage.shared.Utils.FSM)

type Estado = "Idle" | "Run" | "Attack"

const Zombie = Classes.Controller("Zombie")

function Zombie:Setup()
	self.Vida = 100
	self.Machine = FSM.new("Idle" :: Estado) :: FSM.FSM<Estado>
	self.Dependencies.Skeleton:ShotArrow()
end

function Zombie:Heal(Amount: number): boolean
	self.Vida += Amount
	return true
end

return Zombie
```

## O que ele escreve

`Zombie/Type.luau`:

```lua
--!strict
-- GERADO por modux a partir de src/Entity/shared/Zombie/init.luau
-- NAO EDITAR A MAO: a proxima geracao sobrescreve.

local ReplicatedStorage = game:GetService("ReplicatedStorage")
local FSM = require(ReplicatedStorage.shared.Utils.FSM)

type Estado = "Idle" | "Run" | "Attack"

export type Public = {
	Machine: FSM.FSM<Estado>,
	Vida: number,
	Setup: (self: Public) -> (),
	Heal: (self: Public, Amount: number) -> boolean,
}

return {}
```

E a entrada no Manifest, com a dependência que ele viu no corpo:

```lua
Zombie: SelfOf.Build<Zombie.Public, { Skeleton: Skeleton.Public }>,
```

Você não declarou `Require`. A dependência sai do uso de `self.Dependencies.X`.
O `Require` que você escrever continua sendo lido, mas só como override manual
da ordem de load.

## Regras

**Literal alarga.** `self.Vida = 100` vira `Vida: number`, não `Vida: 100`.
Transcrito cru viraria singleton e a segunda atribuição ao mesmo campo quebraria.

**O resto pede `::`.** `self.Orientation = CFrame.new()` não tem tipo que o
gerador possa ler. Anote:

```lua
self.Orientation = CFrame.new() :: CFrame
```

Sem a anotação ele avisa e pula o campo — e aí a tabela selada rejeita a
atribuição, porque o campo não existe na folha.

**Tipo de módulo é referenciado; tipo local é copiado.** Se o tipo vem de um
`require`, a folha requer o mesmo módulo e nunca fica velha. Se está declarado no
corpo, a folha copia — obrigatório, porque a folha não pode requerer o corpo sem
fechar ciclo.

**Caminho relativo sobe um nível.** No corpo (`init.luau`) o `script` **é** a
pasta; na folha (`Type.luau`) o `script` é o arquivo. `require(script.X)` do corpo
vira `require(script.Parent.X)` na folha, automaticamente.

## Desempenho

Cache de extração por arquivo, chaveado por mtime e tamanho. Cada invocação do
`luau-ast` custa dezenas de milissegundos e o binário aceita um arquivo por vez,
então reextrair tudo a cada save custaria N vezes isso.

| | |
|---|---|
| regerar folha e Manifest depois de um save | ~57 ms |
| build frio, 3 módulos | ~130 ms |

## Licença

MIT
