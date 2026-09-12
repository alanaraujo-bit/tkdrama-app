//! Fonte DramaBox (dramabox.com).
//!
//! Diferencas que moldam este modulo, todas verificadas contra o site:
//!
//! - E Next.js **Pages Router**, como o ReelShort: os dados vem inteiros em
//!   `<script id="__NEXT_DATA__">`, sem os pedacos `self.__next_f.push` que o
//!   CandyJar exige.
//! - A midia e **MP4 progressivo assinado** (CloudFront: `?Expires=...&
//!   Signature=...`), nao HLS. E o unico caso alem do TikTok, e por isso este
//!   modulo cai no `download::download_episode` — o mesmo caminho de CDNs
//!   alternativos — em vez do mux de playlist do ReelShort/CandyJar.
//! - A URL do video ja vem na **listagem da serie**: uma unica requisicao
//!   traz nome, capa, sinopse e o `mp4` de todo episodio liberado. Nem o
//!   ReelShort nem o CandyJar fazem isso — os dois exigem uma requisicao por
//!   episodio. Por isso aqui `resolve_media` so reconsulta a mesma pagina,
//!   para reassinar uma URL que expirou.
//! - O caminho da URL e decorativo: `/{lang}/video/{bookId}`,
//!   `/{lang}/video/{bookId}_{slug}/{chapterId}_Episode-N` e
//!   `/{lang}/drama/{bookId}` devolvem os tres a MESMA carga completa.
//!   Verificado: so o `bookId` importa.
//! - `duration` vem em **milissegundos** (154526 = 154s), diferente de todas
//!   as outras fontes, que declaram segundos.
//! - As imagens vem com um sufixo de redimensionamento (`...jpg@w=100&h=135`).
//!   Removido, o mesmo caminho entrega o original — a capa sai de 32 KB para
//!   86 KB e a miniatura de 2,5 KB para 98 KB. O contrato pede resolucao
//!   original, entao o sufixo e sempre removido.
//!
//! Paywall: a listagem ja diz `unlock` por episodio, decidido pelo SERVIDOR a
//! partir da sessao (cookie) de quem pediu — igual ao ReelShort e ao CandyJar.
//! Esse calculo nunca acontece aqui: so se le o que a origem respondeu, e o
//! episodio trancado simplesmente nao traz o campo `mp4`.
//!
//! Sem trailer: a origem nao publica um. `chapterList` e so episodio, todos
//! numerados a partir de `index` 0 — nao existe o item de numero 0 que o
//! ReelShort e o CandyJar usam para o video de apresentacao.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::media::{Episode, Gear, Series, Source, Theme, Upstream, UA};

const BASE: &str = "https://www.dramabox.com";
const DEFAULT_LANG: &str = "en";

/// Nome da unica variante que este modulo resolve.
///
/// A origem publica um MP4 so por episodio, sem lista de qualidades: nao ha o
/// que escolher. O nome descreve a forma (progressivo, ja com audio), nao a
/// resolucao — o `720p` que aparece no nome do arquivo do CDN e um rotulo dele,
/// e ler dimensao de nome de arquivo seria chute. Largura e altura reais saem
/// da medicao do MP4 depois de baixado.
pub const GEAR_NAME: &str = "web_mp4";

pub const PAID_HINT: &str = "episódio pago no DramaBox";
pub const BLOCKED_HINT: &str = "indisponível no DramaBox";

/// Reconhece uma URL do DramaBox e extrai (idioma, id numerico da serie).
///
/// Aceita as formas que o site publica — `/{lang}/video/{bookId}_{slug}/
/// {chapterId}_Episode-N` (a que o usuario copia do player),
/// `/{lang}/drama/{bookId}` e a canonica curta `/{lang}/video/{bookId}` que o
/// `series_url` gera — porque o manifesto grava a canonica e a varredura
/// precisa conseguir le-la de volta.
pub fn parse_url(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim().trim_matches('"');
    if !raw.contains("dramabox.com") {
        return None;
    }
    let after_host = raw.split("dramabox.com").nth(1).unwrap_or("");
    let path = after_host.split(['?', '#']).next().unwrap_or("");
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    // O id da serie e o primeiro trecho depois de "video" ou "drama"; o slug
    // que costuma vir grudado nele (`42000000639_Nome-Da-Novela`) e ignorado.
    let kind_pos = segments
        .iter()
        .position(|s| *s == "video" || *s == "drama")?;
    let raw_id = *segments.get(kind_pos + 1)?;
    let id = raw_id.split('_').next().unwrap_or("");
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    // O prefixo de idioma so existe quando vem ANTES de "video"/"drama".
    let lang = if kind_pos == 0 {
        DEFAULT_LANG.to_string()
    } else {
        segments
            .first()
            .filter(|s| s.len() <= 5 && s.chars().all(|c| c.is_ascii_alphabetic()))
            .map(|s| s.to_string())
            .unwrap_or_else(|| DEFAULT_LANG.to_string())
    };

    Some((lang, id.to_string()))
}

fn series_url(lang: &str, id: &str) -> String {
    format!("{BASE}/{lang}/video/{id}")
}

// --------------------------------------------------------------- rede

/// Puxa o JSON que o Next.js embute na pagina.
fn next_data(html: &str) -> Result<Value> {
    const OPEN: &str = r#"<script id="__NEXT_DATA__" type="application/json">"#;
    let start = html
        .find(OPEN)
        .ok_or_else(|| anyhow!("pagina do DramaBox sem dados embutidos"))?
        + OPEN.len();
    let rest = &html[start..];
    let end = rest
        .find("</script>")
        .ok_or_else(|| anyhow!("dados do DramaBox truncados"))?;
    serde_json::from_str(&rest[..end]).context("dados do DramaBox ilegiveis")
}

async fn get_page(client: &reqwest::Client, url: &str, cookie: &str) -> Result<Value> {
    // Com o cookie do usuario o site responde como para a conta dele (mais
    // episodios liberados). Sem ele, responde como para um visitante — os dois
    // casos sao validos, e o servidor e quem decide.
    let mut req = client
        .get(url)
        .header("User-Agent", UA)
        .header("Accept-Language", "pt-BR,pt;q=0.9,en;q=0.8")
        .header("Referer", BASE);
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    let resp = req
        .send()
        .await
        .context("falha de rede ao consultar o DramaBox")?;
    let status = resp.status();
    let body = resp.text().await.context("resposta ilegivel")?;
    if !status.is_success() {
        bail!("DramaBox respondeu HTTP {}", status.as_u16());
    }
    next_data(&body)
}

// ------------------------------------------------------------------ leitura

fn as_u64(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn as_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn as_bool(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0) != 0,
        _ => false,
    }
}

/// Remove o sufixo de redimensionamento das imagens do CDN.
///
/// `.../capa.jpg@w=360&h=640` entrega um recorte pequeno; o mesmo caminho sem
/// o `@...` entrega o original, que e o que o contrato pede. O `@` so aparece
/// nesse sufixo, entao cortar nele e seguro.
fn full_size_image(url: &str) -> String {
    url.split('@').next().unwrap_or(url).trim().to_string()
}

// ------------------------------------------------------------------- series

pub async fn fetch_series(
    client: &reqwest::Client,
    lang: &str,
    id: &str,
    cookie: &str,
) -> Result<Series> {
    let data = get_page(client, &series_url(lang, id), cookie).await?;
    let pp = data
        .pointer("/props/pageProps")
        .ok_or_else(|| anyhow!("Nao encontrei essa novela no DramaBox."))?;
    let book = pp
        .get("bookInfo")
        .filter(|v| v.is_object())
        .ok_or_else(|| anyhow!("Nao encontrei essa novela no DramaBox."))?;
    let chapters = pp
        .get("chapterList")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("Essa pagina do DramaBox nao lista episodios."))?;

    let mut episodes: Vec<Episode> = Vec::new();
    for c in chapters {
        // `index` e base 0 na origem; o numero do episodio e base 1 em todo o
        // resto do app.
        let number = as_u64(c.get("index")) as u32 + 1;
        let mp4 = as_str(c.get("mp4"));
        let unlocked = as_bool(c.get("unlock"));

        // Trancado: o servidor nao mandou a URL. E o caso normal do paywall.
        // Liberado mas sem `mp4` acontece quando a origem serve aquele
        // episodio por HLS (`m3u8Flag`) — forma que este modulo nao baixa, e
        // cujo campo de playlist nunca foi visto para ser lido. Nao se inventa
        // o campo: marca-se como indisponivel, com o motivo separado.
        let hls_only = unlocked && mp4.is_empty() && as_bool(c.get("m3u8Flag"));
        let locked = !unlocked || mp4.is_empty();

        episodes.push(Episode {
            number,
            item_id: as_str(c.get("id")),
            thumb: full_size_image(&as_str(c.get("cover"))),
            // A origem declara `utime` como data ("2026-07-02"), nao epoch, e
            // e a data de atualizacao do arquivo, nao a estreia. Nao serve
            // como `created_at`: melhor faltar do que afirmar errado.
            created_at: 0,
            is_preview: false,
            is_free_intro: false,
            // Milissegundos na origem; segundos em todo o resto do app.
            duration: (as_u64(c.get("duration")) / 1000) as u32,
            gears: if locked {
                Vec::new()
            } else {
                vec![Gear {
                    gear_name: GEAR_NAME.into(),
                    codec: "h264".into(),
                    // Dimensao e tamanho so se sabem depois de baixar: a
                    // listagem nao os declara, e o `720p` do nome do arquivo do
                    // CDN e rotulo dele, nao medicao.
                    width: 0,
                    height: 0,
                    bitrate: 1,
                    size: 0,
                    urls: vec![mp4.clone()],
                }]
            },
            locked,
            lock_reason: if !locked {
                String::new()
            } else if hls_only {
                BLOCKED_HINT.to_string()
            } else {
                PAID_HINT.to_string()
            },
        });
    }

    if episodes.is_empty() {
        bail!("Nenhum episodio encontrado nessa novela do DramaBox.");
    }
    episodes.sort_by_key(|e| e.number);

    // Temas: `typeTwoList` e a unica lista com identificador estavel. O `name`
    // vem no idioma da pagina pedida (`/pt/` responde "Amor Proibido"), entao
    // o par nao e uma taxonomia em ingles como a do TikTok — o `id` e que se
    // mantem igual entre idiomas. Vai cru: traduzir ou escolher genero e
    // decisao editorial de quem exibe a biblioteca.
    let themes = book
        .get("typeTwoList")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .map(|t| Theme {
                    key: as_str(t.get("id")),
                    value: as_str(t.get("name")),
                    ..Default::default()
                })
                .filter(|t| !t.key.is_empty() || !t.value.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let total_duration_sec = chapters
        .iter()
        .map(|c| as_u64(c.get("duration")) / 1000)
        .sum::<u64>() as u32;

    let name = {
        let n = as_str(book.get("bookName"));
        if n.is_empty() {
            format!("dramabox-{id}")
        } else {
            n
        }
    };

    // `chapterCount` e o que a origem declara ter; `episodes` e o que ela
    // entregou nesta resposta. Guardar o declarado (e nao o contado) faz uma
    // listagem incompleta aparecer como lacuna na interface, em vez de uma
    // serie curta que parece inteira.
    let declared = as_u64(book.get("chapterCount")) as u32;

    Ok(Series {
        drama_id: id.to_string(),
        name,
        description: as_str(book.get("introduction")),
        covers: {
            let cover = full_size_image(&as_str(book.get("cover")));
            if cover.is_empty() {
                Vec::new()
            } else {
                vec![cover]
            }
        },
        total: declared.max(episodes.len() as u32),
        episodes,
        source: Source::Dramabox,
        lang: lang.to_string(),
        themes,
        total_duration_sec,
        upstream: Upstream {
            num_watched: as_u64(book.get("viewCount")),
            collect_count: as_u64(book.get("followCount")),
            ..Default::default()
        },
        is_limited_free: false,
        // A origem nao publica video de apresentacao.
        trailer: None,
    })
}

// --------------------------------------------------------------------- midia

/// Reassina a URL de um episodio.
///
/// As URLs do CDN sao assinadas e expiram (`?Expires=...&Signature=...`), entao
/// a que foi lida na listagem morre depois de um tempo na fila. Como a origem
/// entrega TODAS as URLs na mesma pagina, reconsultar a serie basta: nao existe
/// pagina de episodio a visitar, ao contrario do ReelShort e do CandyJar.
pub async fn resolve_media(
    client: &reqwest::Client,
    lang: &str,
    id: &str,
    number: u32,
    locked: bool,
    cookie: &str,
) -> Result<String> {
    if locked {
        bail!("{PAID_HINT}");
    }
    let series = fetch_series(client, lang, id, cookie).await?;
    let ep = series
        .episodes
        .iter()
        .find(|e| e.number == number)
        .ok_or_else(|| anyhow!("episodio {number} nao existe mais"))?;

    // Barreira do paywall, igual ao ReelShort e ao CandyJar: o servidor decide,
    // aqui so se le o que ele respondeu AGORA — o direito pode ter mudado desde
    // que a tarefa entrou na fila.
    if ep.locked {
        bail!(
            "{}",
            if ep.lock_reason.is_empty() {
                PAID_HINT
            } else {
                &ep.lock_reason
            }
        );
    }
    let url = ep
        .gears
        .first()
        .and_then(|g| g.urls.first())
        .cloned()
        .unwrap_or_default();
    if url.is_empty() {
        bail!("episodio {number} sem midia acessivel");
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconhece_a_url_do_player_que_o_usuario_copia() {
        let (lang, id) = parse_url(
            "https://www.dramabox.com/pt/video/42000000639_Goleada-pelo-Rival-do-Meu-Irmao-Dublado/700040712_Episode-1",
        )
        .unwrap();
        assert_eq!(lang, "pt");
        assert_eq!(id, "42000000639");
    }

    /// A canonica curta e a que o manifesto grava: se `parse_url` nao a
    /// aceitasse, a varredura nao reconheceria a propria pasta de volta.
    #[test]
    fn reconhece_a_url_canonica_curta_que_o_app_gera() {
        let gerada = crate::media::series_url(Source::Dramabox, "42000000639", "pt");
        let (lang, id) = parse_url(&gerada).unwrap();
        assert_eq!(lang, "pt");
        assert_eq!(id, "42000000639");
    }

    #[test]
    fn reconhece_a_pagina_de_serie_e_sem_prefixo_de_idioma() {
        let (lang, id) = parse_url("https://www.dramabox.com/pt/drama/42000000639").unwrap();
        assert_eq!((lang.as_str(), id.as_str()), ("pt", "42000000639"));

        // Sem prefixo de idioma o site usa o default dele.
        let (lang, id) = parse_url("https://www.dramabox.com/video/42000000639").unwrap();
        assert_eq!((lang.as_str(), id.as_str()), (DEFAULT_LANG, "42000000639"));
    }

    #[test]
    fn recusa_url_de_outra_fonte() {
        assert!(parse_url("https://candyjar.com/pt/series/725").is_none());
        assert!(parse_url("https://www.reelshort.com/pt/movie/123").is_none());
        assert!(parse_url("https://www.dramabox.com/pt/video/nao-numerico").is_none());
        assert!(parse_url("https://www.dramabox.com/pt/").is_none());
    }

    #[test]
    fn tira_o_recorte_da_imagem_e_deixa_o_original() {
        assert_eq!(
            full_size_image("https://cdn.x.com/a/capa.jpg@w=360&h=640"),
            "https://cdn.x.com/a/capa.jpg"
        );
        // Sem sufixo, nada muda.
        assert_eq!(
            full_size_image("https://cdn.x.com/a/capa.jpg"),
            "https://cdn.x.com/a/capa.jpg"
        );
        assert_eq!(full_size_image(""), "");
    }

    // ------------------------------------------------------------- rede real

    /// Listagem, paywall e forma da midia contra o site de verdade.
    #[tokio::test]
    #[ignore = "rede: bate no dramabox.com"]
    async fn lista_a_serie_de_verdade() {
        let client = reqwest::Client::new();
        let s = fetch_series(&client, "pt", "42000000639", "").await.unwrap();

        assert_eq!(s.drama_id, "42000000639");
        assert!(!s.name.is_empty(), "novela sem nome");
        assert!(!s.description.is_empty(), "novela sem sinopse");
        assert_eq!(s.source, Source::Dramabox);
        assert!(s.trailer.is_none(), "o DramaBox nao publica trailer");

        // A capa precisa ser a original, sem o recorte do CDN.
        let capa = s.covers.first().expect("novela sem capa");
        assert!(!capa.contains('@'), "capa veio recortada: {capa}");

        // Deslogado, a origem libera so o comeco: e exatamente o paywall que o
        // app precisa respeitar, e nao um erro.
        let livres = s.episodes.iter().filter(|e| e.available()).count();
        assert!(livres > 0, "nenhum episodio liberado");
        assert!(livres < s.episodes.len(), "esperava paywall para visitante");

        let e1 = &s.episodes[0];
        assert_eq!(e1.number, 1, "numeracao comeca em 1");
        assert!(e1.duration > 30 && e1.duration < 3600, "duracao em segundos: {}", e1.duration);
        assert!(e1.gears[0].urls[0].contains(".mp4"), "midia progressiva");
        assert!(!e1.thumb.contains('@'), "miniatura veio recortada");

        // Trancado nunca traz midia, e sempre diz por que.
        let preso = s.episodes.iter().find(|e| e.locked).unwrap();
        assert!(preso.gears.is_empty());
        assert!(!preso.lock_reason.is_empty());
    }

    /// A serie longa e onde uma listagem paginada truncaria em silencio: a
    /// interface do site pagina de 50 em 50, mas a carga do SSR vem inteira.
    #[tokio::test]
    #[ignore = "rede: bate no dramabox.com"]
    async fn serie_longa_vem_inteira_e_nao_so_a_primeira_pagina() {
        let client = reqwest::Client::new();
        let s = fetch_series(&client, "pt", "41000116473", "").await.unwrap();
        assert!(s.total > 50, "esperava novela com mais de 50 episodios");
        assert_eq!(
            s.episodes.len() as u32,
            s.total,
            "listagem truncada na primeira pagina"
        );
    }

    /// A URL assinada precisa servir o arquivo por Range, sem cookie nem
    /// Referer — e o que o downloader progressivo assume para retomar.
    #[tokio::test]
    #[ignore = "rede: bate no CDN do dramabox"]
    async fn a_midia_aceita_retomada_por_range() {
        let client = reqwest::Client::new();
        let url = resolve_media(&client, "pt", "42000000639", 1, false, "")
            .await
            .unwrap();
        let resp = client
            .get(&url)
            .header("User-Agent", UA)
            .header("Range", "bytes=0-999")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 206, "CDN sem suporte a Range");
        let total = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next().map(|s| s.to_string()))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap();
        assert!(total > 100_000, "episodio pequeno demais: {total} bytes");
    }

    /// Episodio pago nunca resolve midia, mesmo se pedirem.
    #[tokio::test]
    #[ignore = "rede: bate no dramabox.com"]
    async fn episodio_trancado_nao_resolve_midia() {
        let client = reqwest::Client::new();
        let s = fetch_series(&client, "pt", "42000000639", "").await.unwrap();
        let preso = s.episodes.iter().find(|e| e.locked).unwrap();
        let err = resolve_media(&client, "pt", "42000000639", preso.number, false, "")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("DramaBox"), "motivo pouco claro: {err}");
    }
}
