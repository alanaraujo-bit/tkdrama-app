//! Fonte ReelShort (reelshort.com).
//!
//! Diferencas que moldam este modulo, todas verificadas contra o site:
//!
//! - O site e Next.js e nao expoe API publica: os dados vem do JSON embutido
//!   em `<script id="__NEXT_DATA__">` de cada pagina.
//! - A pagina da serie lista os capitulos (`online_base`) mas NAO traz a URL do
//!   video. Ela so aparece na pagina do episodio, uma requisicao por episodio.
//!   Por isso a URL e resolvida na hora de baixar, e nao ao listar: com 93
//!   episodios, resolver tudo na listagem seria 93 requisicoes por busca.
//! - So o `book_id` e o `chapter_id` importam nas URLs; titulo e numero no
//!   caminho sao decorativos (testado). Nao e preciso guardar slug.
//! - A midia e HLS aberto: playlist .m3u8 com segmentos .ts, sem `EXT-X-KEY`,
//!   sem DRM. Baixar = juntar os segmentos na ordem.
//!
//! Paywall: o campo `paid_start` diz a partir de qual episodio o conteudo e
//! pago. Esses episodios sao listados como bloqueados e nunca tem a midia
//! resolvida. O site chega a devolver a URL deles para quem nao pagou, mas isso
//! e uma falha de controle de acesso do site, nao permissao: o cadeado e a
//! plataforma dizendo que aquilo se compra.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::media::{Episode, Gear, Series, Source, UA};

const BASE: &str = "https://www.reelshort.com";
const DEFAULT_LANG: &str = "pt";

/// Credenciais da sessao do usuario.
///
/// So o cookie nao basta: a API do ReelShort autentica por um token que o site
/// guarda e envia em cabecalho. Sem ele as chamadas chegam como visitante e o
/// servidor devolve apenas o conteudo gratuito — foi o que travou a primeira
/// tentativa do portao de direito.
#[derive(Clone, Default, Debug)]
pub struct Session {
    pub cookie: String,
    /// Pares (cabecalho, valor) capturados da sessao logada do usuario.
    pub headers: Vec<(String, String)>,
}

impl Session {
    /// Aplica cookie e credenciais a uma requisicao.
    fn apply(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if !self.cookie.is_empty() {
            req = req.header("Cookie", &self.cookie);
        }
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req
    }
}

/// Nome da unica variante que o site entrega pela web.
pub const GEAR_NAME: &str = "hls_sd";

pub const PAID_HINT: &str = "episodio pago no ReelShort";

/// Reconhece uma URL do ReelShort e extrai (idioma, book_id).
///
/// Aceita tanto a pagina da serie (`/pt/movie/<slug>-<book_id>`) quanto a de um
/// episodio (`/pt/episodes/episode-1-<slug>-<book_id>-<chapter_id>`), porque o
/// usuario pode colar qualquer uma das duas.
pub fn parse_url(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim().trim_matches('"');
    if !raw.contains("reelshort.com") {
        return None;
    }
    // Caminho depois do host.
    let after_host = raw.split("reelshort.com").nth(1).unwrap_or("");
    let path = after_host.split(['?', '#']).next().unwrap_or("");
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    // O book_id e um token de 24 digitos hexadecimais em qualquer posicao.
    let book_id = segments
        .iter()
        .flat_map(|seg| seg.split('-'))
        .find(|tok| tok.len() == 24 && tok.chars().all(|c| c.is_ascii_hexdigit()))?
        .to_string();

    // Primeiro segmento e o idioma quando nao e uma secao conhecida.
    let lang = segments
        .first()
        .filter(|s| s.len() <= 5 && !matches!(**s, "movie" | "episodes"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| DEFAULT_LANG.to_string());

    Some((lang, book_id))
}

fn series_url(lang: &str, book_id: &str) -> String {
    format!("{BASE}/{lang}/movie/{book_id}")
}

fn episode_url(lang: &str, book_id: &str, number: u32, chapter_id: &str) -> String {
    format!("{BASE}/{lang}/episodes/episode-{number}-x-{book_id}-{chapter_id}")
}

/// Puxa o JSON que o Next.js embute na pagina.
fn next_data(html: &str) -> Result<Value> {
    const OPEN: &str = r#"<script id="__NEXT_DATA__" type="application/json">"#;
    let start = html
        .find(OPEN)
        .ok_or_else(|| anyhow!("pagina do ReelShort sem dados embutidos"))?
        + OPEN.len();
    let rest = &html[start..];
    let end = rest
        .find("</script>")
        .ok_or_else(|| anyhow!("dados do ReelShort truncados"))?;
    serde_json::from_str(&rest[..end]).context("dados do ReelShort ilegiveis")
}

async fn get_page(client: &reqwest::Client, url: &str, sess: &Session) -> Result<Value> {
    // Com a sessao do usuario o site responde como para a conta dele. Sem ela,
    // responde como para um visitante — os dois casos sao validos.
    let req = sess.apply(
        client
            .get(url)
            .header("User-Agent", UA)
            .header("Accept-Language", "pt-BR,pt;q=0.9,en;q=0.8")
            .header("Referer", BASE),
    );
    let resp = req
        .send()
        .await
        .context("falha de rede ao consultar o ReelShort")?;
    let status = resp.status();
    let body = resp.text().await.context("resposta ilegivel")?;
    if !status.is_success() {
        bail!("ReelShort respondeu HTTP {}", status.as_u16());
    }
    next_data(&body)
}

fn as_u64(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn as_str(v: Option<&Value>) -> String {
    v.and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// Pergunta ao servidor quais capitulos ESTA sessao pode assistir.
///
/// Este e o portao de direito do app, e ele e do servidor, nao meu. A API
/// `getChapterList` devolve `play_info` (a informacao de reproducao) apenas
/// para os capitulos que a conta tem direito: anonimamente, os episodios pagos
/// vem sem ela. Entao "tem play_info" == "esta conta pode assistir", decidido
/// pelo ReelShort e apenas lido aqui.
///
/// E melhor que o `paid_start` que eu usava antes por dois motivos: libera o
/// que o VIP do usuario cobre, e continua bloqueando para quem nao tem — o
/// corte fixo nao sabia diferenciar uma coisa da outra.
///
/// Devolve os numeros de episodio liberados. `None` quando a API nao respondeu:
/// nesse caso o chamador mantem o criterio antigo em vez de liberar no escuro.
async fn entitled_episodes(
    client: &reqwest::Client,
    book_id: &str,
    sess: &Session,
) -> Option<std::collections::HashSet<u32>> {
    const PAGE_SIZE: u32 = 50;
    let mut liberados = std::collections::HashSet::new();
    let mut viu_algum = false;

    for page in 1..=20 {
        let url = format!(
            "{BASE}/api/video/book/getChapterList?book_id={book_id}&page={page}&page_size={PAGE_SIZE}"
        );
        let Ok(resp) = get_json(client, &url, sess).await else {
            // Falhou logo na primeira pagina: nao da para afirmar nada.
            return if viu_algum { Some(liberados) } else { None };
        };
        if as_u64(resp.get("code")) != 0 {
            return if viu_algum { Some(liberados) } else { None };
        }

        // O nome da colecao ja variou entre versoes da API; aceita os dois.
        let itens = resp
            .pointer("/data/list")
            .or_else(|| resp.pointer("/data/chapter_list"))
            .or_else(|| resp.pointer("/data/data"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if itens.is_empty() {
            break;
        }
        viu_algum = true;

        for c in &itens {
            let numero = as_u64(c.get("serial_number")) as u32;
            if numero == 0 {
                continue; // trailer
            }
            let tem_play_info = c
                .get("play_info")
                .map(|v| match v {
                    Value::String(s) => s.trim().len() > 10,
                    Value::Null => false,
                    outro => !outro.is_null(),
                })
                .unwrap_or(false);
            if tem_play_info {
                liberados.insert(numero);
            }
        }

        if (itens.len() as u32) < PAGE_SIZE {
            break;
        }
    }

    if viu_algum {
        Some(liberados)
    } else {
        None
    }
}

/// Lista a serie inteira. Episodios pagos entram marcados como trancados, sem
/// midia — resolver a URL deles seria furar o paywall.
pub async fn fetch_series(
    client: &reqwest::Client,
    lang: &str,
    book_id: &str,
    sess: &Session,
    // Direito ja apurado pela janela logada, quando disponivel. Tem prioridade
    // sobre a consulta feita daqui: ela sai de dentro da sessao autenticada.
    direito: Option<std::collections::HashSet<u32>>,
    // VIP ativo: libera a serie inteira, independentemente do resto.
    vip_total: bool,
) -> Result<Series> {
    let data = get_page(client, &series_url(lang, book_id), sess).await?;
    let d = data
        .pointer("/props/pageProps/data")
        .ok_or_else(|| anyhow!("Nao encontrei essa novela no ReelShort."))?;

    let name = as_str(d.get("book_title"));
    let chapters = d
        .get("online_base")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("Essa pagina do ReelShort nao lista episodios."))?;

    // `paid_start` = primeiro episodio pago. E um corte fixo, igual para todo
    // mundo: nao sabe quem esta perguntando. So vale como plano B.
    let is_paid = as_u64(d.get("is_paid")) == 1;
    let paid_start = as_u64(d.get("paid_start")) as u32;

    // Preferencia sempre: o que o SERVIDOR diz que esta sessao pode assistir.
    // Primeiro o que a janela logada apurou; se nao houver, tenta daqui.
    let liberados = match direito {
        Some(set) => Some(set),
        None => entitled_episodes(client, book_id, sess).await,
    };

    let mut episodes: Vec<Episode> = Vec::new();
    for c in chapters {
        let number = as_u64(c.get("serial_number")) as u32;
        // serial_number 0 e o trailer, nao um episodio da novela.
        if number == 0 {
            continue;
        }
        let chapter_id = as_str(c.get("chapter_id"));
        if chapter_id.is_empty() {
            continue;
        }
        // VIP ativo libera tudo. Senao, o servidor manda (lista de liberados);
        // so cai no corte fixo se nada respondeu.
        let locked = if vip_total {
            false
        } else {
            match &liberados {
                Some(set) => !set.contains(&number),
                None => is_paid && paid_start > 0 && number >= paid_start,
            }
        };

        episodes.push(Episode {
            number,
            item_id: chapter_id,
            // A lista de capitulos do ReelShort nao traz miniatura nem data de
            // estreia: `getChapterList` devolve `serial_number` e `chapter_id`,
            // e nada mais. Sem miniatura, quem exibe cai na arte gerada — que e
            // melhor que uma inventada.
            thumb: String::new(),
            created_at: 0,
            is_preview: false,
            is_free_intro: false,
            // A duracao real so vem na pagina do episodio; resolvida ao baixar.
            duration: 0,
            // Variante sintetica: as URLs dos segmentos chegam depois, em
            // `resolve_media`. Uma lista vazia aqui significaria "bloqueado".
            gears: if locked {
                Vec::new()
            } else {
                vec![Gear {
                    gear_name: GEAR_NAME.into(),
                    codec: "h264".into(),
                    width: 0,
                    height: 0,
                    bitrate: 1,
                    size: 0,
                    urls: Vec::new(),
                }]
            },
            locked,
            lock_reason: if !locked {
                String::new()
            } else if liberados.is_some() {
                // O servidor foi consultado e nao liberou este episodio.
                "sua conta do ReelShort não liberou este episódio".to_string()
            } else {
                PAID_HINT.to_string()
            },
        });
    }

    if episodes.is_empty() {
        bail!("Nenhum episodio encontrado nessa novela do ReelShort.");
    }
    episodes.sort_by_key(|e| e.number);

    let total = as_u64(d.get("total")) as u32;
    Ok(Series {
        drama_id: book_id.to_string(),
        name: if name.is_empty() {
            format!("reelshort-{book_id}")
        } else {
            name
        },
        description: as_str(d.get("special_desc")),
        covers: {
            let pic = as_str(d.get("book_pic"));
            if pic.is_empty() {
                Vec::new()
            } else {
                vec![pic]
            }
        },
        total: if total > 0 { total } else { episodes.len() as u32 },
        episodes,
        source: Source::Reelshort,
        lang: lang.to_string(),
        // O ReelShort nao publica temas, duracao total nem contadores de
        // audiencia na pagina da serie. Ficam vazios, e o manifesto os omite.
        themes: Vec::new(),
        total_duration_sec: 0,
        upstream: Default::default(),
        is_limited_free: false,
    })
}

/// Playlist HLS -> lista ordenada de segmentos absolutos.
///
/// Aceita tambem a playlist mestre (varias qualidades), escolhendo a de maior
/// banda e seguindo para a playlist de midia dela.
pub async fn fetch_segments(
    client: &reqwest::Client,
    m3u8_url: &str,
    sess: &Session,
) -> Result<Vec<String>> {
    let text = get_text(client, m3u8_url, sess).await?;

    if text.contains("#EXT-X-STREAM-INF") {
        let variant = best_variant(&text, m3u8_url)
            .ok_or_else(|| anyhow!("playlist mestre do ReelShort sem variantes"))?;
        let text = get_text(client, &variant, sess).await?;
        return media_segments(&text, &variant);
    }
    media_segments(&text, m3u8_url)
}

/// Chama uma API JSON do site com a sessao do usuario.
async fn get_json(client: &reqwest::Client, url: &str, sess: &Session) -> Result<Value> {
    let req = sess.apply(
        client
            .get(url)
            .header("User-Agent", UA)
            .header("Accept", "application/json, text/plain, */*")
            .header("Accept-Language", "pt-BR,pt;q=0.9,en;q=0.8")
            .header("Referer", BASE),
    );
    let resp = req.send().await.context("falha de rede na API do ReelShort")?;
    let status = resp.status();
    let body = resp.text().await.context("resposta ilegivel")?;
    if !status.is_success() {
        bail!("HTTP {}", status.as_u16());
    }
    serde_json::from_str(&body).context("API do ReelShort nao devolveu JSON")
}

async fn get_text(client: &reqwest::Client, url: &str, sess: &Session) -> Result<String> {
    let req = sess.apply(client.get(url).header("User-Agent", UA).header("Referer", BASE));
    let resp = req
        .send()
        .await
        .context("falha de rede ao buscar a playlist")?;
    let status = resp.status();
    let body = resp.text().await.context("playlist ilegivel")?;
    if !status.is_success() {
        bail!("playlist respondeu HTTP {}", status.as_u16());
    }
    Ok(body)
}

/// Resolve uma referencia da playlist contra a URL de onde ela veio.
fn absolute(reference: &str, base: &str) -> String {
    if reference.starts_with("http://") || reference.starts_with("https://") {
        return reference.to_string();
    }
    if let Some(rest) = reference.strip_prefix('/') {
        // Caminho absoluto: mantem so o esquema + host da base.
        let root = base
            .split_once("://")
            .and_then(|(scheme, rest)| {
                rest.split_once('/').map(|(host, _)| format!("{scheme}://{host}"))
            })
            .unwrap_or_else(|| base.to_string());
        return format!("{root}/{rest}");
    }
    let dir = base.split(['?', '#']).next().unwrap_or(base);
    match dir.rfind('/') {
        Some(i) => format!("{}/{}", &dir[..i], reference),
        None => reference.to_string(),
    }
}

fn media_segments(text: &str, base: &str) -> Result<Vec<String>> {
    if text.contains("#EXT-X-KEY") {
        // Nao ha caso conhecido no ReelShort, mas se aparecer e melhor falhar
        // claro do que gravar um arquivo de lixo criptografado.
        bail!("playlist criptografada: nao da para baixar");
    }
    let segments: Vec<String> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| absolute(l, base))
        .collect();
    if segments.is_empty() {
        bail!("playlist do ReelShort sem segmentos");
    }
    Ok(segments)
}

/// Resolve a midia de um episodio: pagina do episodio -> m3u8 -> segmentos.
///
/// Devolve (segmentos, duracao). Recusa episodio pago: o chamador ja filtra,
/// mas esta e a barreira que garante que nenhum caminho novo passe por cima.
pub async fn resolve_media(
    client: &reqwest::Client,
    lang: &str,
    book_id: &str,
    number: u32,
    chapter_id: &str,
    locked: bool,
    sess: &Session,
) -> Result<(Vec<String>, u32)> {
    if locked {
        bail!("{PAID_HINT}");
    }
    let url = episode_url(lang, book_id, number, chapter_id);
    let data = get_page(client, &url, sess).await?;
    let d = data
        .pointer("/props/pageProps/data")
        .ok_or_else(|| anyhow!("episodio {number} nao existe mais"))?;

    let video_url = as_str(d.get("video_url"));
    if video_url.is_empty() {
        bail!("episodio {number} sem midia acessivel");
    }
    let duration = as_u64(d.get("duration")) as u32;
    let segments = fetch_segments(client, &video_url, sess).await?;
    Ok((segments, duration))
}

fn best_variant(text: &str, base: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().map(|l| l.trim()).collect();
    let mut best: Option<(u64, String)> = None;
    for (i, line) in lines.iter().enumerate() {
        if !line.starts_with("#EXT-X-STREAM-INF") {
            continue;
        }
        let bandwidth = line
            .split("BANDWIDTH=")
            .nth(1)
            .map(|s| {
                s.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
            })
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        if let Some(url) = lines
            .get(i + 1)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            if best.as_ref().map(|(b, _)| bandwidth > *b).unwrap_or(true) {
                best = Some((bandwidth, absolute(url, base)));
            }
        }
    }
    best.map(|(_, u)| u)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOK: &str = "69d8612ad6461266e80b7a17";

    #[test]
    fn reconhece_as_duas_formas_de_url() {
        let (lang, id) = parse_url(&format!(
            "https://www.reelshort.com/pt/episodes/episode-1-ligados-pela-honra-{BOOK}-nzolf08ec8"
        ))
        .unwrap();
        assert_eq!(lang, "pt");
        assert_eq!(id, BOOK);

        let (lang, id) =
            parse_url(&format!("https://www.reelshort.com/en/movie/some-title-{BOOK}")).unwrap();
        assert_eq!(lang, "en");
        assert_eq!(id, BOOK);
    }

    #[test]
    fn ignora_o_que_nao_e_reelshort() {
        assert!(parse_url("https://www.tiktok.com/shortdrama/episode/7677/1").is_none());
        // Sem book_id de 24 hex nao da para montar nada.
        assert!(parse_url("https://www.reelshort.com/pt/movie/sem-id").is_none());
    }

    #[test]
    fn urls_relativas_da_playlist_viram_absolutas() {
        let base = "https://cdn.x.com/vod/abc/def-sd.m3u8";
        assert_eq!(absolute("seg-1.ts", base), "https://cdn.x.com/vod/abc/seg-1.ts");
        assert_eq!(absolute("/raiz/s.ts", base), "https://cdn.x.com/raiz/s.ts");
        let abs = "https://outro.com/s.ts";
        assert_eq!(absolute(abs, base), abs);
    }

    #[test]
    fn le_segmentos_na_ordem() {
        let pl = "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:5.0,\na-00001.ts\n#EXTINF:5.0,\na-00002.ts\n#EXT-X-ENDLIST\n";
        let segs = media_segments(pl, "https://cdn.x.com/v/p.m3u8").unwrap();
        assert_eq!(segs.len(), 2);
        assert!(segs[0].ends_with("a-00001.ts"));
        assert!(segs[1].ends_with("a-00002.ts"));
    }

    #[test]
    fn playlist_criptografada_falha_em_vez_de_gravar_lixo() {
        let pl = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.key\"\n#EXTINF:5.0,\na.ts\n";
        assert!(media_segments(pl, "https://cdn.x.com/v/p.m3u8").is_err());
    }

    #[test]
    fn playlist_mestre_escolhe_a_maior_banda() {
        let pl = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000\nlow.m3u8\n\
                  #EXT-X-STREAM-INF:BANDWIDTH=2400000\nhigh.m3u8\n";
        let v = best_variant(pl, "https://cdn.x.com/v/master.m3u8").unwrap();
        assert!(v.ends_with("high.m3u8"));
    }

    /// Ponta a ponta contra o site de verdade: lista, respeita o paywall,
    /// resolve a midia do episodio 1 e grava um arquivo integro.
    ///
    /// Marcado `ignore` por depender da rede e baixar alguns MB. Rodar com:
    ///   cargo test --lib reelshort -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn ponta_a_ponta_baixa_episodio_livre() {
        use crate::download::{download_hls, Outcome, RUN};
        use std::sync::atomic::AtomicU8;
        use std::sync::Arc;

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(20))
            .read_timeout(std::time::Duration::from_secs(45))
            .build()
            .unwrap();

        let series = fetch_series(&client, "pt", BOOK, &Session::default(), None, false)
            .await
            .expect("lista a serie");
        println!("serie: {} ({} episodios)", series.name, series.episodes.len());

        // O paywall precisa estar refletido na lista.
        let ep11 = series.episodes.iter().find(|e| e.number == 11).unwrap();
        let ep12 = series.episodes.iter().find(|e| e.number == 12).unwrap();
        assert!(ep11.available(), "episodio 11 e gratuito e deveria estar liberado");
        assert!(ep12.locked, "episodio 12 e pago e deveria estar trancado");
        assert!(ep12.gears.is_empty(), "episodio pago nao pode carregar midia");

        let ep1 = series.episodes.iter().find(|e| e.number == 1).unwrap();
        let (segments, duration) =
            resolve_media(&client, "pt", BOOK, 1, &ep1.item_id, ep1.locked, &Session::default())
                .await
                .expect("resolve a midia do episodio 1");
        println!("segmentos: {} | duracao: {duration}s", segments.len());
        assert!(segments.len() > 5);

        let dir = std::env::temp_dir().join("tkdrama_rs_e2e");
        let _ = std::fs::remove_dir_all(&dir);
        let dest = dir.join("E01.ts");
        let control = Arc::new(AtomicU8::new(RUN));

        let outcome = download_hls(&client, &segments, &dest, "", control, |_, _| {})
            .await
            .expect("baixa o episodio");

        let bytes = std::fs::metadata(&dest).unwrap().len();
        println!("gravado: {bytes} bytes");
        assert!(matches!(outcome, Outcome::Done(n) if n == bytes));
        assert!(bytes > 500_000, "arquivo pequeno demais: {bytes}");

        // MPEG-TS valido comeca com o byte de sincronismo 0x47, e ele se repete
        // a cada 188 bytes. Isso separa "baixou" de "baixou algo que presta".
        let head = std::fs::read(&dest).unwrap();
        assert_eq!(head[0], 0x47, "primeiro byte nao e sincronismo de TS");
        for pacote in 1..50 {
            assert_eq!(head[pacote * 188], 0x47, "sincronismo perdido no pacote {pacote}");
        }
        // O .part e o indice de retomada nao podem sobrar depois de concluir.
        assert!(!crate::download::part_path(&dest).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// O portao de direito precisa vir do servidor, e este teste prova que ele
    /// funciona: sem sessao, o ReelShort so entrega `play_info` dos episodios
    /// livres. Se um dia a API parar de distinguir, este teste quebra — e e
    /// exatamente isso que eu quero saber antes de liberar algo indevido.
    ///
    ///   cargo test --lib reelshort -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn portao_de_direito_vem_do_servidor() {
        let client = reqwest::Client::new();
        let set = entitled_episodes(&client, BOOK, &Session::default())
            .await
            .expect("a API respondeu");

        let mut liberados: Vec<u32> = set.iter().copied().collect();
        liberados.sort_unstable();
        println!("liberados sem sessao: {liberados:?}");

        // Anonimo enxerga exatamente os gratuitos (1..=11).
        for n in 1..=11 {
            assert!(set.contains(&n), "episodio {n} e gratuito e deveria estar liberado");
        }
        for n in [12, 13, 40, 93] {
            assert!(!set.contains(&n), "episodio {n} e pago e NAO pode vir liberado sem sessao");
        }

        // E a serie montada precisa refletir esse portao.
        let series = fetch_series(&client, "pt", BOOK, &Session::default(), None, false)
            .await
            .expect("lista");
        let ep12 = series.episodes.iter().find(|e| e.number == 12).unwrap();
        assert!(ep12.locked && ep12.gears.is_empty());
        assert!(series.episodes.iter().find(|e| e.number == 11).unwrap().available());
    }

    #[tokio::test]
    async fn episodio_pago_nunca_resolve_midia() {
        let client = reqwest::Client::new();
        // `locked` = true corta antes de qualquer requisicao.
        let r = resolve_media(
            &client,
            "pt",
            BOOK,
            12,
            "10mdp166yi",
            true,
            &Session::default(),
        )
        .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("pago"));
    }
}
