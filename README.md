# tkdrama

App de desktop para Windows que baixa novelas verticais (*short dramas*) do
TikTok e do ReelShort e guarda em disco, para assistir offline, em qualquer
player, a qualquer momento.

Nenhuma das duas plataformas oferece download dessas séries nem expõe a lista de
episódios como playlist. O app descobre os episódios sozinho pela API interna,
usando a sessão do próprio usuário, e baixa tudo em fila — o que hoje só existe
como trabalho manual, episódio por episódio.

## O que ele faz

- **Descobre a série inteira a partir de um link.** Cola-se a URL da novela (ou
  a de um episódio) e o app pagina a API até o último episódio, com a numeração
  correta.
- **Baixa em fila**, com prioridade, pausa/retomada e até 3 downloads
  simultâneos — ou um de cada vez, em ordem, no modo sequencial.
- **Acompanha novelas sozinho.** Aponta-se a série uma vez e o app a revisita de
  tempos em tempos, enfileirando o episódio novo assim que ele existe. É o que
  dispensa voltar à plataforma toda semana para colar o mesmo link.
- **Retomada real:** downloads sobrevivem a fechar o app, queda de rede e
  desligamento. Nada recomeça do zero.
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

Não precisa de ffmpeg: os episódios do TikTok já vêm como MP4 único com áudio, e
os do ReelShort são segmentos MPEG-TS emendados pelo próprio app.

## Rodando

```bash
npm install
npm run tauri dev
```

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
   sozinhos. Em *Ajustes* ficam o intervalo e os limites (novelas e GB por
   rodada, piso de espaço livre em disco).

Cada novela vira uma subpasta com o nome dela, e os arquivos saem numerados
(`Nome da novela - E01.mp4`).

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
  tiktok.rs     cliente da API de short dramas do TikTok
  reelshort.rs  cliente do ReelShort (HLS)
  media.rs      tipos comuns e escolha de qualidade
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
