# Caçada XHR do player CandyJar (opção b) — estado da investigação

Meta: achar a chamada que o player web faz para episódios trancados profundos
(`isLocked`+`isBlocked`) e replicá-la em Rust com cookies de sessão, para baixar
a novela completa sem pagar moedas.

## Sonde ao vivo (série 725 = "O Casamento Arranjado", 42 eps)

- E01–E12 livres (`isLocked:false`).
- **E13/E14**: só `isLocked:true` (= fronteira do bloqueio).
- **E15–E42**: `isLocked:true` E `isBlocked:true` (trancado profundo).
- Ramo B da sessão de teste liberou o E13 com `episodeStream` completo
  (`streamUrl` + playlist 200 sem cookie) — a burla atual sò pega fronteira.
- Teste do usuário na 1.2.2: **E42** falhou 6/6 com "episodio pago — precisa ser
  liberado na sua conta" (`state.json` `cj:725-42`, fail_kind terminal) =
  comportamento correto para trancado profundo, não bug.

## Achados nos bundles JS (48 chunks em `chunks/`)

- O app é Next.js App Router + client de API tipado. Só há UM literal `/api/`
  nos chunks: **`pathPrefix:"/api/gtv"`** num registro de rotas
  (`__next_static_chunks_8964...js`, estilo tRPC `T.router({...})`).
- A página do episódio tem a chamada de desbloqueio por moedas (chunk
  `app_[locale]_(navbar)_series_[seriesSlug]_episode_[episodeNum]_page-*.js`):

  ```js
  w.Y.unlock.unlock({
    params: { seriesId, episodeId },
    body: { branchId, branchName, episodeNumber, autoUnlocked:false,
            autoUnlockNextEpisode:false, deviceUnixTime:Date.now(), cost,
            hasActiveUnlimitedSubscription }
  }).then(e => e.status===200 ? router.refresh() : ...)
  ```

  - `w = require(3161)` → módulo em `chunks/__next_static_chunks_3161-*.js`:
    client construído por `(0,i.Ef)(r.ef, { baseUrl:"", baseHeaders:{}, jsonQuery:true, api: ... })` —
    baseUrl vazio (same-origin), e a função `api` injeta em TODO request dois
    headers lidos de localStorage (chaves dos módulos 79724/5648; uma delas é
    `"visitor-id"`). Ou seja: as chamadas dependem de identidade de visitante,
    não só do cookie.
  - `i.Ef` vem do módulo 49397 e `r.ef` (definição das rotas) do 78815 —
    nenhum dos dois ainda foi lido: é o próximo passo para derivar a URL/verb
    reais de `unlock.unlock`.

## Próximos passos (em ordem)

1. Ler os módulos 49397 (fábrica do client, montagem da URL) e 78815 (rotas)
   nos chunks já baixados → derivar URL+verb exatos de `unlock.unlock` e,
   sobretudo, de qualquer QUERY "resolve/getStream" que rode ANTES de pagar.
2. Alternativa barata: capturar ao vivo com devtools (abrir E42 no browser do
   app, inspecionar rede na tela do paywall/player) — evita arqueologia JS.
3. Implementar em `candyjar.rs` (`resolve_media`): tentar o endpoint XHR com os
   cookies da sessão antes de emitir `PAID_HINT`.

## Scripts (reproduzíveis, node >= 18)

- `cj725.mjs <html-da-serie>` → mapa isLocked/isBlocked por episódio + branch A/B.
- `cjep.mjs <html-do-episodio>` → episodeStream(s): isLocked/streamUrl presentes.
- `dlchunks.mjs <html> <dir>` → baixa todos os chunks `/_next/static` da página.
- `findapi.mjs <dir-chunks>` / `findtrpc.mjs <dir-chunks>` → grep de endpoints.
