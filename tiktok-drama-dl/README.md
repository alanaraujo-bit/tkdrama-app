# tkdrama

Baixa novelas verticais (short dramas) do TikTok em fila, com retomada,
numeração correta de episódios e escolha de qualidade.

Descobre todos os episódios de uma série sozinho — você só passa a URL.

## Requisitos

- Python 3 (já instalado: 3.14)
- Nenhuma dependência externa (só a biblioteca padrão)
- **ffmpeg não é necessário** — os episódios já vêm como mp4 único com áudio

## Configuração (uma vez)

1. Abra `tiktok.com` no navegador, **logado**.
2. Instale a extensão **Get cookies.txt LOCALLY** (roda 100% local).
3. Com o TikTok aberto, clique na extensão → **Export** → formato **Netscape**.
4. Salve o arquivo como `cookies.txt` nesta pasta.

O script filtra sozinho: só usa cookies de `tiktok.com` e ignora os de outros
sites, mesmo que você exporte o arquivo completo. Também corrige o formato de
domínio que as extensões costumam gravar errado.

## Uso

Baixar a série inteira:

```
py tkdrama.py "https://www.tiktok.com/shortdrama/episode/7677784436146164743/1"
```

Ver o que existe, sem baixar:

```
py tkdrama.py 7677784436146164743 --list
```

Só alguns episódios, em outra pasta:

```
py tkdrama.py 7677784436146164743 --episodes 1-10,25 --out "D:/Novelas"
```

Aceita tanto a URL completa quanto só o ID da série.

### Opções

| Opção | O que faz |
|---|---|
| `-o, --out` | pasta de destino (padrão: `./<nome da série>`) |
| `-c, --cookies` | caminho do cookies.txt (padrão: `cookies.txt`) |
| `-e, --episodes` | filtro, ex: `1-10,15,20-25` |
| `--codec` | `h264` (padrão) ou `h265` |
| `--gear` | força uma variante exata, ex: `adapt_lowest_1080_1` |
| `--delay` | pausa entre requisições (padrão: 1.0s) |
| `-l, --list` | só lista, não baixa |

## Sobre a qualidade

O TikTok oferece 7 variantes por episódio. A maior **resolução** não é a melhor
**qualidade** — o 1080p é upscale com bitrate *menor* que o 540p:

| Variante | Codec | Resolução | Bitrate |
|---|---|---|---|
| `normal_540_0` | h264 | 576×1024 | **662 kbps** ← padrão |
| `adapt_lowest_1080_1` | h265 | 1080×1920 | 587 kbps |
| `normal_480_0` | h264 | 480×854 | 515 kbps |
| `adapt_lower_720_1` | h265 | 720×1280 | 471 kbps |

Por isso o padrão é `normal_540_0`: é o maior bitrate real, e h264 toca em
qualquer player. Se quiser o quadro maior mesmo perdendo detalhe, use
`--codec h265` ou `--gear adapt_lowest_1080_1`.

## Como funciona

1. Extrai o `dramaID` da URL.
2. Pagina `GET /api/drama/episode/item_list/` (24 por vez) até juntar tudo.
3. Numera pelo campo `DramaVideoData.EpisodeNumber` — a ordem real da série,
   não a ordem em que a API devolve.
4. Escolhe a variante de maior bitrate no codec preferido.
5. Baixa em fila com `Range`, validando o tamanho contra o `DataSize` declarado.

Grava um `manifest.json` na pasta de saída com o que foi baixado e em que
qualidade.

## Detalhes que importam

**Retomada.** O download vai para `.part` e só vira `.mp4` quando o tamanho
bate exatamente. Interrompeu (Ctrl+C, queda de rede, PC desligou)? Rode o mesmo
comando: os prontos são pulados e os incompletos continuam de onde pararam.

**Múltiplos CDNs.** Cada episódio traz 3 URLs. Se uma falhar ou a assinatura
expirar (403), ele tenta a próxima automaticamente.

**Rodar de novo é seguro.** Nunca rebaixa o que já está completo.

**Episódios bloqueados.** Se um episódio exigir desbloqueio, ele vem sem mídia
acessível. O script pula, avisa, e segue o resto — depois lista quais foram no
resumo final. Ele baixa o que a sua conta já pode assistir; não contorna
bloqueio.

**Sessão expira.** Cookie do TikTok dura algumas semanas. Quando vencer, o erro
é explícito e diz para reexportar o `cookies.txt`. É só repetir a configuração.

## Privacidade

`cookies.txt` é a sua sessão do TikTok — quem tem o arquivo entra na sua conta.
Já está no `.gitignore` junto com os vídeos. Não compartilhe.
