# tkdrama

App de desktop para Windows que baixa novelas verticais (*short dramas*) do
TikTok, do ReelShort, do CandyJar e do DramaBox e guarda em disco, para assistir
offline, em qualquer player, a qualquer momento.

Nenhuma dessas plataformas oferece download dessas séries nem expõe a lista de
episódios como playlist. O app descobre os episódios sozinho pela API interna,
usando a sessão do próprio usuário, e baixa tudo em fila — o que hoje só existe
como trabalho manual, episódio por episódio.

## O que ele faz

- **Descobre a série inteira a partir de um link.** Cola-se a URL da novela (ou
  a de um episódio) e o app pagina a API até o último episódio, com a numeração
  correta.
- **Adiciona muitas novelas de uma colagem só.** *Adicionar em lote* aceita
  texto solto — uma coluna de links, um bloco de conversa, HTML copiado de uma
  página — acha os links no meio, deduplica por novela e processa a lista
  inteira, já enfileirando e já marcando cada uma como acompanhada. É o que
  troca trinta idas à plataforma por uma. A rodada respeita o mesmo ritmo do
  acompanhamento automático (20s entre uma novela e outra), então ela anuncia
  onde está e pode ser cancelada.
- **Baixa em fila**, com prioridade, pausa/retomada e até 3 downloads
  simultâneos — ou um de cada vez, em ordem, no modo sequencial.
- **Acompanha novelas sozinho.** Aponta-se a série uma vez e o app a revisita de
  tempos em tempos, enfileirando o episódio novo assim que ele existe. É o que
  dispensa voltar à plataforma toda semana para colar o mesmo link.
- **Retomada real:** downloads sobrevivem a fechar o app, queda de rede e
  desligamento. Nada recomeça do zero.
- **Aguenta a noite sozinho.** Uma fila grande é deixada rodando sem ninguém
  olhando, e é isso que o desenho protege:
  - **Nunca baixa o mesmo episódio duas vezes.** Quem enfileira consulta a pasta
    por *número* de episódio — não pelo nome que o downloader escreveria hoje —
    então um arquivo renomeado, ou vindo de quando a novela tinha outro título,
    continua contando como "já tenho". Colar o link de uma novela que já está no
    disco avisa quantos você tem e enfileira só os que faltam.
  - **Queda de rede não vira muro de erros.** Quando as falhas são de rede o
    agendador fecha o portão e para de puxar tarefa nova, em vez de queimar as
    seis tentativas de cada uma em quatro minutos. Uma sonda testa a conexão a
    cada 15s e reabre o portão sozinha quando ela volta.
  - **Zelador de dez em dez minutos**, que devolve à fila o que falhou por
    motivo passageiro (rede, link assinado vencido, disco cheio) — até doze
    vezes por episódio, cerca de duas horas de recuperação. O que é definitivo
    (pago, fora do ar, sessão expirada) não gira em falso.
  - **A máquina não suspende enquanto há fila.** É o defeito que nenhuma
    retentativa alcança: fila imortal em computador suspenso não baixa nada. O
    pedido é só "não suspenda" — a tela apaga normalmente — e é devolvido assim
    que a fila esvazia.
- **Deixa a ficha da novela na pasta.** Junto dos vídeos vão um `manifest.json`,
  a capa (`poster.jpg`) e a miniatura de cada episódio (`thumbs/E##.jpg`) — o que
  permite a um app de biblioteca exibir o catálogo sem falar com a plataforma.
  As URLs de imagem são assinadas e expiram, então a imagem é **baixada**, nunca
  anotada. *Ajustes → Atualizar metadados* faz o mesmo para o que já está no
  disco: converte `.ts` legados, normaliza MP4, gera miniaturas ausentes e
  atualiza o manifesto sem rebaixar vídeo nenhum.
- **Escolhe a qualidade pelo bitrate, não pela resolução** — ver abaixo.
- Fica na bandeja do sistema: fechar a janela não interrompe a fila.

## Duas verdades técnicas que explicam o desenho

**Maior resolução não é maior qualidade.** O 1080p deste catálogo é upscale, com
bitrate menor que o do 540p. O app escolhe por bitrate, e a tela de qualidade
explica isso — senão a escolha parece errada.

**As URLs de mídia são assinadas e expiram.** Por isso elas nunca são
persistidas: ao iniciar uma tarefa sem URL válida, a série é consultada de novo.
É também por isso que o acompanhamento automático enfileira por ID de série, e
nunca por URL já resolvida.

## Requisitos

- Windows 10 ou 11
- [Node.js](https://nodejs.org) 20.19+ ou 22.12+ (exigência do Vite 8)
- [Rust](https://rustup.rs) e o [WebView2](https://developer.microsoft.com/microsoft-edge/webview2/),
  que já vem no Windows 11
- Uma conta na plataforma de onde se vai baixar

Não precisa instalar FFmpeg: o executável necessário já acompanha o app. Os
episódios do TikTok e do DramaBox vêm como MP4; os do ReelShort são baixados
como MPEG-TS e remuxados sem perda para MP4, e os do CandyJar chegam em HLS e
são empacotados pelo FFmpeg. Se os codecs não forem H.264/AAC, o FFmpeg os
converte. O arquivo só aparece como concluído após validação integral e
`faststart`; até lá usa os sufixos `.part`/`.parcial.mp4`.

## Rodando

```bash
npm install
npm run tauri dev
```

Os binários LGPL do FFmpeg não ficam no repositório — são 128 MB e o histórico
do Git não os perderia mais. Antes do primeiro build, preencha
`src-tauri/binaries/` conforme o `README.md` daquela pasta, que traz a versão
exata, a URL de origem e o SHA-256.

Para gerar o instalador (NSIS, em `src-tauri/target/release/bundle/`):

```bash
npm run tauri build
```

## Usando

1. Clique no *chip* de login no topo e entre na sua conta. A sessão fica no
   perfil do WebView2 do app e sobrevive ao fechamento — é uma vez só.
2. Cole o link da novela e clique em **Buscar**.
3. **Baixar tudo** — ou **Escolher episódios**, se quiser só alguns.
4. Opcionalmente, **Acompanhar**: a partir daí os episódios novos entram na fila
   sozinhos. Em *Ajustes → Automático* ficam o intervalo e os limites (novelas e
   GB por rodada, piso de espaço livre em disco), junto da conta de quanto cada
   rodada leva com esses limites.

Para muitas novelas de uma vez, pule os passos 2 a 4: **Adicionar em lote**,
cole tudo o que você juntou, confira a prévia (ela roda sem rede) e mande
adicionar. Com *Acompanhar* marcado — o padrão — a lista passa a se manter
sozinha.

Cada novela vira uma subpasta com o nome dela, e os arquivos saem numerados
(`Nome da novela - E01.mp4`).

### Backfill e validação pela linha de comando

O botão **Ajustes → Atualizar metadados** é o caminho normal. Para automação,
a CLI usa o mesmo pipeline local (o argumento pode ser uma novela ou a raiz da
biblioteca):

```powershell
cargo run --manifest-path src-tauri/Cargo.toml --features cli --bin tkdrama-media -- backfill "D:\Novelas"
cargo run --manifest-path src-tauri/Cargo.toml --features cli --bin tkdrama-media -- validate "D:\Novelas\Nome da Novela"
```

Também há um comando reproduzível para baixar episódios gratuitos de uma URL
do ReelShort sem abrir a interface (TikTok continua usando a sessão do app):

```powershell
cargo run --manifest-path src-tauri/Cargo.toml --features cli --bin tkdrama-media -- download "URL_DA_NOVELA" "D:\Novelas" 2
```

## Limites deliberados

- **Baixa só o que a sua conta já pode assistir.** Não contorna DRM (não há),
  nem paywall, nem bloqueio por moeda. Episódio fora do alcance da conta aparece
  na lista como bloqueado, com o motivo — nunca como download. Isso vale
  igualmente para o que entra pelo acompanhamento automático.
- **Não escolhe novela sozinho.** O automático reabastece o que você já apontou
  uma vez; a descoberta de novela nova acontece na plataforma. A pasta só contém
  o que você escolheu.
- **Sem player embutido.** O objetivo é ter o arquivo, assistido onde você
  quiser.
- O que entra sozinho tem prioridade baixa: nunca passa na frente de um download
  pedido na mão.

## Segurança

A janela de login carrega o site externo e por isso **não recebe nenhuma
permissão do app** (`src-tauri/capabilities/default.json`, `windows: ["main"]`).
Conteúdo externo nunca enxerga as APIs nativas.

Os cookies de sessão ficam no perfil local do WebView2 e nunca saem da máquina.
Não há conta, servidor nem sincronização: é um app de um usuário só, rodando na
própria máquina.

## Estrutura

```
src/            interface (TypeScript, sem framework)
src-tauri/src/
  lib.rs        comandos Tauri, autenticação, janelas
  queue.rs      motor da fila: prioridade, concorrência, persistência
  download.rs   download com retomada
  watch.rs      novelas acompanhadas e reabastecimento automático
  bulk.rs       muitas novelas de uma colagem só: extrai, deduplica, processa
  awake.rs      impede a máquina de suspender enquanto a fila tem trabalho
  tiktok.rs     cliente da API de short dramas do TikTok
  reelshort.rs  cliente do ReelShort (HLS)
  candyjar.rs   cliente do CandyJar (HLS, fMP4/CMAF)
  dramabox.rs   cliente do DramaBox (MP4 progressivo assinado)
  media.rs      tipos comuns e escolha de qualidade
  manifest.rs   ficha da novela em disco: manifest.json, capa, miniaturas
  backfill.rs   varredura que dá ficha ao que já foi baixado
  disk.rs       espaço livre em disco
tiktok-drama-dl/  protótipo anterior em Python, que originou o app
```

Rodando os testes:

```bash
cd src-tauri && cargo test
```

## Licença

[MIT](LICENSE) — © 2026 Alan Araujo.

Ferramenta de uso pessoal. O respeito aos termos de uso das plataformas e aos
direitos do conteúdo baixado é responsabilidade de quem usa.
