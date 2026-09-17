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
rokit install
```

Na primeira vez o Rokit pede para você confirmar que confia na ferramenta:

```
ERROR The following tool has not been marked as trusted: victorcarmo2003/modux
Run `rokit add victorcarmo2003/modux` to install and trust this tool.
```

É só rodar o mesmo comando de novo num terminal interativo e aceitar. Em CI, use
`rokit trust victorcarmo2003/modux` antes do `rokit install`.

Não tem mais nada para instalar: o parser de Luau é compilado dentro do binário.
Versões até a 0.1 dependiam do `luau-ast` no PATH, o que não dava para resolver com
o Rokit — ele guarda um binário por ferramenta, e no pacote `luau-lang/luau` esse
binário é o `luau`.

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

Cache de extração por arquivo, chaveado por mtime e tamanho, para não reparsear
o projeto inteiro a cada save.

| | |
|---|---|
| build frio, 7 módulos, folhas e os dois Manifests | ~80 ms |
| `modux check` nos mesmos 7 módulos | ~78 ms |

Esses números são do cronômetro interno e não contam a partida do processo, que
no Windows sozinha custa ~150 ms de relógio. Em `watch` o processo já está de pé,
então o que você sente é o número da tabela.

## Licença

MIT
