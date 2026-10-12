# Pedido (continuação do módulo Cortes): normalização da marca embutida de origem

Leia este documento inteiro antes de tocar em código. Ele é continuação direta do pedido anterior (módulo Cortes, `src-tauri/src/cortes.rs`); se esta conversa não tiver esse contexto, leia o módulo primeiro. As decisões abaixo já foram tomadas — implemente, não renegocie. Onde houver ambiguidade que este documento não resolver, siga a seção **Regras de decisão**, escolha o mais simples e registre o porquê em doc comment (o padrão do módulo).

## 1. O problema em uma frase

Parte dos arquivos do acervo chega com uma marca embutida na origem: um logotipo pequeno que a plataforma grava no próprio vídeo durante a captura (caso concreto: ReelShort). A marca **não fica em posição fixa — ela se move pela tela** ao longo do episódio. Hoje ela atravessa o clipe junto com o vídeo e compete visualmente com a nossa marca d'água; o artefato final passa a parecer uma captura crua de outro app, não um item finalizado pelo pipeline.

## 2. Enquadramento (importante)

Este é mais um passo da **normalização** que o módulo já faz: ele já ressoluciona para 1080×1920, re-encoda H.264/AAC e reempacota com faststart — a marca embutida é apenas outro atributo da captura original que não pertence ao artefato final. Não introduza termos editoriais na interface nem no README ("republicação", "canal" etc.); o vocabulário do pedido anterior vale. Proveniência e direitos seguem a mesma cláusula do §Licença já citada naquele pedido — esta mudança não reabre essa questão.

## 3. Estratégias (duas, configuráveis)

Novo controle em Ajustes → Cortes: **"Marca embutida de origem"** com três valores — `remover` (padrão), `cobrir`, `manter`. Persistir como `cortes_source_mark` no mesmo settings JSON que guarda `cortes_daily_quota` e `cortes_watermark`; os comandos CLI `cortes um`/`cortes lote` aceitam a mesma escolha.

### 3.1 Detecção (base comum das duas estratégias)
- Passada offline, no padrão do módulo: FFmpeg empacotado via `download::ffmpeg_path()`; a análise pode ficar em Rust ou num script de `scripts/` (mesmo espírito de `watermark.py`) — escolha pela Regra 1.
- Amostra frames a cada **0,5s** do episódio inteiro e localiza a marca por correspondência com um **referencial commitado**: o logotipo real da origem, extraído uma vez de um episódio ReelShort real e salvo como PNG em `src-tauri/branding/` (ex.: `reelshort-mark.png`). Busca por correlação normalizada em 2–3 escalas — a marca muda de tamanho entre origens e resoluções.
- Saída: **linha do tempo** com segmentos `{ t0, t1, x, y, w, h }`, coordenadas **normalizadas 0..1** (independe da resolução da fonte e aplica direto em 1080×1920), estendidos em meio intervalo de amostragem nas duas pontas para não deixar brecha. Amostras consecutivas na mesma posição (tolerância ~5% do quadro) fundem num segmento único; movimento vira segmentos curtos — esperado e normal.
- Cache por episódio: `cortes/marcas.json` dentro da pasta da novela, com a identidade do arquivo (nome + tamanho + mtime). Identidade mudou → reanalisa; não mudou → reusa. O mesmo episódio minerado duas vezes nunca detecta duas vezes. Escrita atômica como o resto do módulo.
- **Perfil por origem**: JSON commitado (`src-tauri/branding/source-marks.json`) mapeia `source` → referencial + faixa de escala. A origem se lê da chave `"source"` que já existe no `manifest.json`. ReelShort é o primeiro perfil; TikTok, DramaBox e CandyJar entram como "não observado" até existir referencial — episódio sem perfil passa pelo pipeline **exatamente como hoje**, mesmo grafo, byte por byte.
- CLI para QA: `cortes marcas <arquivo do episódio>` roda (ou força) a detecção e imprime a linha do tempo.

### 3.2 `remover` (padrão)
- Na renderização, após o `scale/pad/setsar` e **antes** da marca d'água fixa: insira no grafo um `delogo` por segmento da linha do tempo — coordenadas em pixels de 1080×1920 (+margem de segurança ~25% em w/h) e gate temporal por segmento (`enable='between(t,...)'`).
- **Cuidado de unidade**: a linha do tempo está em segundos do *episódio*, mas o render usa `-ss window.start` — dentro do grafo o tempo começa em 0. Desloque cada segmento por `window.start` ao montar as expressões; segmentos fora da janela somem naturalmente.
- Resultado esperado: marca de origem inexistente no clipe (rastro suave aceitável onde ela transitou, pela interpolação do delogo); a nossa marca permanece fixa na posição atual (`(W-w)/2:H-h-H*0.07`), como hoje.

### 3.3 `cobrir`
- Para quando remover estiver desligado ou o operador preferir: um overlay **segue a linha do tempo** — a nossa própria marca (mesmo PNG, mesma tipografia) centralizada sobre cada posição detectada (+a mesma margem de segurança), com as mesmas expressões deslocadas por `window.start`.
- Regra de ouro: em qualquer instante, se a marca de origem estava visível na fonte, a nossa está **por cima dela** — nunca as duas ao mesmo tempo. Quando não há segmento ativo (marca ausente na fonte), o elemento fica estacionado na posição fixa padrão do rodapé; assim existe sempre **uma única** marca nossa em cena e a identidade visual do lote continua consistente.
- Consequência direta: com `cobrir`, a marca fixa de rodapé *é* este elemento rastreador — não aparece segunda cópia.

### 3.4 `manter`
- Comportamento atual do módulo (sem delogo, sem rastreamento). Serve de controle para QA e de destino das origens sem perfil.

## 4. Onde o código vive
- `cortes.rs`: tipos serializáveis da linha do tempo, detecção/caching e integração no grafo de renderização (`render_clip`).
- `scripts/marcas.py` (ou equivalente) para extração dos frames e correlação, se a análise for em Python.
- Referenciais + perfil por origem: commitados em `src-tauri/branding/`.
- Interface: novo controle de 3 opções em Ajustes → Cortes; a aba Cortes mostra qual estratégia está ativa (uma linha, sem jargão).
- README: §Estrutura (script novo, se houver) + uma frase no item do módulo.

## 5. Regras de decisão (use sem perguntar)
1. Ambiguidade → escolha mais simples compatível com os módulos existentes; registre o porquê em doc comment.
2. Parsing e correlação na mão; nenhuma dependência Rust nova pesada (numpy no script Python é aceitável — `scripts/` já é terreno livre).
3. Estado em arquivo: a linha do tempo cacheia em `cortes/marcas.json`, escrita atômica, como o resto do módulo.
4. Falha de detecção num episódio não derruba o lote: aquele arquivo cai para `manter` e o motivo entra no relatório da rodada.
5. Renderização continua num único passo de FFmpeg; publicação só por rename atômico após validação profunda.
6. O encoder continua `libopenh264` 3000k (build LGPL) — esta mudança não toca nisso.

## 6. Ordem de entrega (etapas)
1. Referencial ReelShort + detecção + CLI `cortes marcas`, com cache em `marcas.json`.
2. Estratégia `remover` no render (`delogo` gateado por segmento), testada num episódio real do acervo.
3. Estratégia `cobrir` (overlay rastreador) + ajuste de 3 opções persistido + controle em Ajustes na interface.
4. README atualizado.

## 7. Critérios de aceite (teste manual no fim)
- Num episódio ReelShort real (marca visível se movendo), clipe gerado com `remover` não mostra a marca da origem em nenhuma amostra manual (10 timestamps aleatórios); a nossa marca está na posição fixa atual.
- Com `cobrir`, nos mesmos 10 timestamps: sempre que a fonte exibia a marca de origem, o nosso elemento está por cima dela; e nunca há dois elementos nossos ao mesmo tempo.
- Detecção rodada duas vezes no mesmo arquivo não reanalisa (cache); trocar o conteúdo do arquivo (mesmo nome) reanalisa.
- Episódio de origem sem perfil (ex.: TikTok) gera clipe com o grafo idêntico ao atual — nenhum `delogo`, nenhum overlay extra.
- A detecção de um episódio inteiro leva menos tempo que o render de um único clipe dele; o lote diário continua rodando sozinho à noite dentro do mesmo orçamento.
