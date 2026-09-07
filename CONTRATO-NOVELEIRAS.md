# Contrato com o Noveleiras de Plantão

Este documento é um pedido de uma ferramenta para a outra.

O **tkdrama** baixa as novelas. O **Noveleiras de Plantão**
(`D:\PROJETOS\Noveleiras de Plantão`) é o app que as exibe: ele varre a pasta
da biblioteca, cria o catálogo e serve os vídeos. Hoje ele encontra os arquivos
e nada mais — sem capa, sem sinopse, sem gênero, sem miniatura de episódio.
A tela fica com arte abstrata gerada e fichas vazias.

O tkdrama **já busca** esses dados na API. Ele só não os grava em disco:
`Series.name`, `Series.description` e `Series.cover` vivem em memória durante o
download e morrem com o processo. O pedido é simples: **persistir na pasta da
novela o que já se sabe sobre ela.**

---

## 1. A regra que decide tudo: URL de imagem não serve

As URLs de capa vêm assinadas e expiram — o mesmo que já vale para as URLs de
mídia, pela razão que o README do tkdrama documenta. Do dump real
(`tiktok-drama-dl/all_eps.json`):

```
dramaInfo.cover.urlList[0]  ->  ...tiktokx-origin.image?dr=14575&x-expires=1788922800&x-signature=...
```

Gravar essa URL num JSON produz uma capa que funciona por alguns dias e depois
vira 404 para sempre. **A imagem precisa ser baixada como arquivo.** O
Noveleiras serve qualquer arquivo dentro da raiz da biblioteca, então uma capa
em disco já é uma capa publicável — sem nenhuma migração de banco do lado dele.

---

## 2. O que gravar

Para cada novela, dentro da pasta que o tkdrama já cria:

```
D:\Novelas\A cura mortal do bilionário\
  A cura mortal do bilionário - E01.mp4
  A cura mortal do bilionário - E02.mp4
  ...
  manifest.json        <- novo
  poster.jpg           <- novo (capa vertical da novela)
  thumbs/
    E01.jpg            <- novo (miniatura do episódio)
    E02.jpg
```

### `poster.jpg`

De `dramaInfo.cover.urlList[0]` (TikTok) ou `book_pic` (ReelShort).

Use `cover.urlList`, **não** `zoomCover`: o `zoomCover` é um recorte quadrado
de 240–960 px, e a capa do app é vertical (720×1080). O `urlList` traz a
imagem em resolução original.

### `thumbs/E##.jpg`

De `video.originCover` de cada episódio. Este é o dado que mais melhora a lista
de episódios, e é genuinamente distinto por episódio.

No ReelShort a lista de capítulos não traz miniatura — nesse caso, sem
`thumbs/`, e o app cai na arte gerada. Melhor faltar do que inventar.

### `manifest.json`

**A forma não é livre.** O Noveleiras já lê um `manifest.json`
(`lib/media/biblioteca.ts`) com estas chaves, e elas precisam continuar
exatamente assim — o resto é aditivo:

```jsonc
{
  // --- já lido hoje pelo Noveleiras: nomes fixos ---
  "dramaID": "7677784436146164743",
  "dramaName": "A cura mortal do bilionário",
  "totalEpisodes": 40,

  // --- novo: nível da novela ---
  "source": "tiktok",                    // "tiktok" | "reelshort"
  "description": "Após prisão, Adeline Jenkins luta para pagar contas...",
  "poster": "poster.jpg",                // caminho relativo à pasta da novela
  "totalDurationSec": 4884,
  "themes": [                            // dramaInfo.themes, cru
    { "key": "tag_Contractlovers_Relationship", "value": "Contract Lovers" },
    { "key": "tag_Marriedlife_Plot",            "value": "Married Life" }
  ],
  "upstream": {                          // números da plataforma de origem
    "numWatched": 25638630,
    "playCount": 384900,
    "diggCount": 20500,
    "collectCount": 6686,
    "commentCount": 220
  },
  "isLimitedFree": true,
  "capturedAt": "2026-09-07T12:00:00Z",  // quando estes metadados foram lidos

  // --- por episódio, indexado pelo número (como já é hoje) ---
  "episodes": {
    "1": {
      "file": "A cura mortal do bilionário - E01.mp4",
      "duration": 160,
      "width": 576,
      "height": 1024,
      "codec": "h264",

      // novo:
      "thumb": "thumbs/E01.jpg",
      "itemId": "7677784436146164743",
      "createdAt": "2026-08-23T04:46:12Z",  // de createTime (epoch -> ISO)
      "isPreview": true,                    // DramaVideoData.IsPreview
      "isFreeIntro": false,                 // DramaVideoData.IsFreeIntro
      "locked": false
    }
  }
}
```

---

## 3. O que **não** existe, e por isso não se deve inventar

O Noveleiras gostaria de título e sinopse por episódio, no estilo Netflix.
**A API não tem isso.** Verificado no dump real: o campo `desc` do episódio 1 e
o do episódio 2 são idênticos — é a chamada da série repetida em todo item, não
a sinopse do episódio. No ReelShort, `getChapterList` devolve `serial_number` e
`chapter_id`, sem nenhum texto.

Então: **não gere `episodes[n].title` nem `episodes[n].synopsis`.** Um resumo
adivinhado engana quem lê o catálogo, e o Noveleiras tem uma tela no painel
para alguém escrever a ficha à mão quando quiser. Campo ausente é honesto;
campo preenchido por chute, não.

O que faz a lista de episódios parecer Netflix sem inventar nada é o que é real
e por episódio: miniatura, duração, data e a marca de prévia/grátis. Isso já
está na seção 2.

---

## 4. O comando que falta: metadados para o que já foi baixado

Há hoje ~396 episódios já baixados, sem manifesto e sem capa. O Noveleiras não
pode buscá-los: ele não fala com o TikTok, e nem deve.

Por isso o pedido central é um comando que **percorra as pastas existentes,
consulte só os metadados da série (sem baixar vídeo nenhum) e escreva
`manifest.json` + `poster.jpg` + `thumbs/`.** Algo como "Atualizar metadados"
na interface, ou um `--metadados-apenas` na linha de comando.

Sem ele, a melhoria só alcança o que for baixado de hoje em diante — e a
biblioteca atual fica sem arte para sempre.

Naturalmente, todo download novo também deve escrever/atualizar o manifesto ao
terminar a série.

---

## 5. Detalhes que evitam retrabalho

**Escrita incremental.** O manifesto precisa ser atualizado a cada episódio
concluído, ou pelo menos ao fim da série. Uma fila interrompida no episódio 20
deve deixar um manifesto com 20 episódios, não nenhum.

**Escrita atômica.** Grave em `manifest.json.tmp` e renomeie. O Noveleiras pode
estar varrendo a pasta no mesmo instante, e um JSON pela metade quebra a
varredura inteira.

**Arquivo do ReelShort.** O tkdrama baixa o HLS para um `.ts` intermediário e
faz remux sem perda antes de publicar o episódio. O contrato entregue ao
Noveleiras é sempre `.mp4`; o manifesto só passa a apontar para ele depois da
publicação atômica. Manifestos antigos continuam sendo lidos pelo número do
episódio, que permanece a identidade estável durante a migração.

**`capturedAt`.** Serve para decidir se vale reconsultar a API. Uma novela em
andamento muda de sinopse e de contagem; uma concluída, não.

**Números de origem ficam isolados em `upstream`.** O Noveleiras tem os
próprios contadores de audiência, alimentados pela telemetria dele e usados no
painel. Misturar o `playCount` do TikTok com o `viewCount` de lá corromperia a
análise. Por isso o bloco separado e nomeado.

**`themes` vai cru, em inglês mesmo.** O mapeamento para os gêneros em
português é uma decisão editorial que acontece no painel do Noveleiras, com
alguém olhando. Não traduza, não invente gênero: mande `key` e `value` como
vieram. O `key` é o identificador estável.

---

## 6. Resumo do pedido

1. Escrever `manifest.json` na pasta de cada novela, na forma da seção 2.
2. Baixar `poster.jpg` da capa da série.
3. Baixar `thumbs/E##.jpg` de cada episódio (TikTok; ReelShort não tem).
4. Um comando que faça 1–3 para as pastas **já baixadas**, sem rebaixar vídeo.
5. Não inventar título nem sinopse de episódio — eles não existem na origem.
