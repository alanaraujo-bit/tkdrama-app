# Pedido: módulo Cortes — clipes do acervo local

Leia este documento inteiro antes de tocar em código. Este é o pedido direto do dono do projeto; as decisões abaixo já foram tomadas — implemente, não renegocie. Onde houver ambiguidade que este documento não resolver, siga a seção **Regras de decisão**, escolha o caminho mais simples e registre o porquê num doc comment (o estilo desta base é explicar a motivação no código).

## 1. Contexto em uma frase

O tkdrama já baixa novelas verticais para `D:\Novelas`, com `manifest.json`, capa e miniaturas por episódio; falta um módulo que transforme episódios do acervo local em clipes curtos, normalizados e marcados — arquivos prontos para o canal vertical externo do operador.

## 2. O que é (e o que não é) este módulo

É um **pipeline de pós-produção sobre arquivos que já estão no disco**: leitura, análise, recorte, normalização e marca d'água. A saída é um MP4 validado numa pasta — nada além disso.

**Não-escopo neste pedido:**
- upload ou publicação em qualquer rede: quem publica é o operador, fora do app;
- legenda automática, música de fundo ou narração: a trilha permanece a original do episódio — não adicione áudio novo;
- escolha de qual novela baixar (regra existente: o app nunca escolhe novela sozinho). Vale igual aqui para *minar*: só episódio já baixado entra na análise.

**Proveniência e direitos:** são responsabilidade do operador — a mesma cláusula que o §Licença do README já declara sobre o download ("o respeito aos termos de uso das plataformas e aos direitos do conteúdo baixado é responsabilidade de quem usa"). Este módulo não reabre essa questão: ele apenas normaliza e marca arquivos locais. Se precisar registrar isso, uma linha no README sob a seção existente basta; **não crie disclaimer na interface**.

## 3. Especificação funcional

### 3.1 Seleção do dia
- Lote diário de N clipes (configurável em Ajustes; padrão **10**).
- Candidatos: episódios já no disco e ainda nunca usados como fonte de clipe (o uso fica registrado — ver estado). O `trailer.mp4` também é candidato.
- Score por episódio = densidade de mudança de cena + variação de energia sonora na janela alvo, medidos com o FFmpeg empacotado (mesmo binário que `download.rs::ffmpeg_path()` localiza): timestamps de cena via filtro `select='gt(scene,0.3)',showinfo` lido do stderr; energia de áudio por análise curta (`astats`/`silencedetect`). Se a análise falhar para um episódio, use fallback determinístico: janela central.
- Recência pesa no score: episódios com `capturedAt`/mtime mais recentes têm vantagem — mas o lote não pode ficar restrito ao novo; quando o pool de "nunca minerado" esvaziar, minar acervo antigo é comportamento esperado, sem aviso.
- Diversidade: no máximo **2 clipes por novela por dia**.

### 3.2 Extração do clipe
- Duração alvo ~45s; aceito entre **30 e 90s**.
- Pontos de corte presos a fronteiras de cena (snap ±1,5s); não cortar no meio de fala quando existir fronteira a menos de 1,5s.
- Fade de áudio de 80ms na entrada e na saída para evitar estouro.

### 3.3 Normalização
- Saída sempre **1080×1920 (9:16)**, H.264 + AAC, `+faststart`, com publicação atômica no padrão dos downloads (`*.part` → rename só depois de validação completa — reutilize o pipeline de validação profunda que já existe).
- Fontes do acervo são 576×1024 (exatamente 9:16): escala pura, sem crop. Se aparecer proporção fora disso, encaixar em 9:16 com padding mínimo.

### 3.4 Marca d'água "Noveleiras de plantão"
- Texto exato: `Noveleiras de plantão` (minúsculo em "de plantão").
- Estilo sucinto e profissional: pill arredondado escuro com ~25% de opacidade atrás, texto branco a 80%, fonte Segoe UI Semibold; altura do conjunto ≈ 3,5% da altura do quadro.
- Posição fixa em todos os clipes: centro horizontal, a ~7% da borda inferior — dentro das margens seguras de interface vertical.
- Implementação: **PNG transparente pré-renderizado**, commitado no repositório (script `scripts/watermark.py` regenerando-o, no mesmo espírito de `make_icon.py`). O pipeline faz `overlay` do PNG em posição fixa: tipografia consistente entre clipe e clipe, sem recalcular a cada render.
- Ajustes: liga/desliga da marca (padrão ligado).

### 3.5 Saída no disco
Dentro de cada pasta de novela, numa subpasta `cortes/` (a listagem de episódios `- E##.mp4` continua intacta para quem varre a biblioteca):

```
D:\Novelas\<Nome da Novela>\cortes\
  <Nome da Novela> - CL-20260911-01.mp4
  cortes.json      <- índice: [{ "file", "sourceEpisode", "startSec", "endSec", "score", "generatedAt" }]
```

### 3.6 Lote diário / agendamento
- Rodada própria, no mesmo estilo do acompanhamento automático (`watch.rs`): dispara no máximo uma vez por dia e tem **prioridade baixa** — nunca compete com download em andamento; fila de download ativa adia o lote até esvaziar.
- Idempotente: reexecutar no mesmo dia completa a cota que falta sem duplicar nada.

### 3.7 Estado
Mesma convenção de persistência que a fila já usa (JSON local, escrita atômica). Registrar por episódio minerado: novela, número do episódio, data e clipe gerado; por rodada: data, quantos gerados e falhas com motivo.

## 4. Onde o código vive
- `src-tauri/src/cortes.rs` — módulo novo (seleção, extração, marca d'água, lote).
- Comandos Tauri registrados em `lib.rs`; aba **Cortes** na interface (`src/main.ts`, mesmo estilo sem framework): resumo do dia (X de N), lista dos clipes gerados com episódio-fonte e duração, botões *Gerar agora* e *Abrir pasta*; em Ajustes: cota diária e liga/desliga da marca.
- CLI para QA sem abrir a interface (mesmo padrão `features = ["cli"]`): `cortes um <arquivo do episódio>` gera um clipe manual de um arquivo; `cortes lote --n 10` roda o lote completo.
- Atualize a lista §Estrutura do README com o novo módulo.

## 5. Regras de decisão (use sem perguntar)
1. Ambiguidade → escolha mais simples compatível com os módulos existentes; registre o porquê em doc comment.
2. Nenhuma dependência Rust nova pesada; parsing da saída do FFmpeg é texto — faça na mão.
3. Estado fica em arquivo (JSON), nunca em banco.
4. Falha num episódio não derruba o lote: logar motivo e seguir (padrão da fila).
5. Publicação só por rename atômico após validação completa; MP4 parcial jamais visível.
6. Entre qualidade de encode e tempo, prefira `crf 20` / preset `veryfast` — o lote roda sozinho à noite.

## 6. Ordem de entrega (etapas)
1. CLI `cortes um <arquivo>` funcionando num episódio real da biblioteca: recorte + normalização + marca d'água, arquivo validado.
2. Seleção por score + lote diário via CLI `cortes lote`, com estado e idempotência.
3. Agendamento integrado à rotina automática do app + aba **Cortes** na interface + ajustes.
4. README atualizado (§Estrutura + uma linha de proveniência sob §Licença, se aplicável).

## 7. Critérios de aceite (teste manual no fim)
- Um clipe gerado a partir de um episódio real tem exatamente 1080×1920, H.264/AAC, `faststart`, duração entre 30 e 90s.
- A marca d'água aparece em todos os clipes, na posição exata especificada, legível sem dominar a cena.
- Rodar o lote duas vezes no mesmo dia não gera clipe duplicado; rodar com cota 10 produz até 10 arquivos em novelas distintas (máx. 2 por novela).
- O `cortes.json` existe e bate 1:1 com os arquivos da pasta.
- Nenhum download em andamento foi interrompido ou retardado visivelmente durante a geração de um lote.
