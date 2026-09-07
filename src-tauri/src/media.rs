//! Modelo de midia comum as fontes suportadas.
//!
//! O app nasceu so com TikTok e os tipos moravam dentro de `tiktok.rs`. Com o
//! ReelShort entrando, eles passam a viver aqui: a fila, a interface e o
//! downloader falam desta linguagem, e cada fonte so cuida de traduzir o que
//! recebe para ela.

use serde::{Deserialize, Serialize};

pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36 Edg/152.0.0.0";

/// De onde veio a serie. Decide quem resolve as URLs e como se baixa.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    #[default]
    Tiktok,
    Reelshort,
}

/// Todas as fontes, para varrer estado por fonte sem esquecer nenhuma.
pub const SOURCES: [Source; 2] = [Source::Tiktok, Source::Reelshort];

impl Source {
    /// Prefixo do id da tarefa. Sem ele, um `book_id` do ReelShort e um
    /// `dramaID` do TikTok poderiam colidir e a fila trataria os dois como a
    /// mesma coisa.
    pub fn tag(&self) -> &'static str {
        match self {
            Source::Tiktok => "tk",
            Source::Reelshort => "rs",
        }
    }

    /// Chave do cache de series. Duas fontes podem usar o mesmo id sem colidir.
    pub fn cache_key(&self, drama_id: &str) -> String {
        format!("{}:{drama_id}", self.tag())
    }

    pub fn label(&self) -> &'static str {
        match self {
            Source::Tiktok => "TikTok",
            Source::Reelshort => "ReelShort",
        }
    }

    /// Onde abrir a janela de login desta fonte.
    pub fn login_url(&self) -> &'static str {
        match self {
            Source::Tiktok => "https://www.tiktok.com/login",
            Source::Reelshort => "https://www.reelshort.com/profile",
        }
    }

    /// Pedaco que identifica um cookie como sendo desta fonte.
    pub fn cookie_domain(&self) -> &'static str {
        match self {
            Source::Tiktok => "tiktok",
            Source::Reelshort => "reelshort",
        }
    }

    /// Rotulo da janela de login. Precisa ser distinto por fonte, senao uma
    /// sobrescreveria a outra — e nenhum deles recebe permissao do app
    /// (capabilities concede so a janela "main").
    pub fn login_window(&self) -> &'static str {
        match self {
            Source::Tiktok => "login",
            Source::Reelshort => "login-reelshort",
        }
    }

    /// Nome do cookie que prova sessao ativa, quando se conhece.
    ///
    /// No TikTok e o `sessionid`. No ReelShort ainda nao se sabe qual e — por
    /// isso `None`, e a interface se limita a dizer quantos cookies existem em
    /// vez de afirmar "conectado" sem ter como provar.
    pub fn session_cookie(&self) -> Option<&'static str> {
        match self {
            Source::Tiktok => Some("sessionid"),
            Source::Reelshort => None,
        }
    }
}

/// Uma variante de qualidade de um episodio.
///
/// No TikTok `urls` sao CDNs alternativos do mesmo arquivo: tenta-se um apos o
/// outro. No ReelShort sao os segmentos HLS em ordem, todos necessarios — por
/// isso quem baixa precisa olhar a `Source` antes de decidir o que fazer.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Gear {
    pub gear_name: String,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub bitrate: u64,
    pub size: u64,
    pub urls: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Episode {
    pub number: u32,
    pub item_id: String,
    pub duration: u32,
    /// Vazio = episodio sem midia acessivel (bloqueado).
    pub gears: Vec<Gear>,
    /// Episodio existente mas fora do alcance da conta (pago/trancado). E
    /// diferente de `gears` vazio: aqui sabe-se por que nao da para baixar.
    #[serde(default)]
    pub locked: bool,
    /// Explicacao curta do bloqueio, mostrada na interface.
    #[serde(default)]
    pub lock_reason: String,

    /// Miniatura propria do episodio (`video.originCover` no TikTok).
    ///
    /// E o dado que mais melhora a lista de episodios de quem exibe, e o unico
    /// genuinamente distinto por episodio. A URL e assinada e expira, entao ela
    /// serve para BAIXAR o arquivo, nunca para ser gravada. O ReelShort nao tem
    /// miniatura de capitulo: aqui fica vazio, e o outro lado cai na arte
    /// gerada — melhor faltar do que inventar.
    #[serde(default)]
    pub thumb: String,
    /// Estreia na origem, em Unix seconds. 0 = a fonte nao declara.
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub is_preview: bool,
    #[serde(default)]
    pub is_free_intro: bool,
}

impl Episode {
    /// Da para baixar? Trancado nunca, mesmo que a midia esteja alcancavel.
    pub fn available(&self) -> bool {
        !self.locked && !self.gears.is_empty()
    }
}

/// Um tema declarado pela origem, cru — `key` estavel, `value` como veio.
///
/// Nao se traduz aqui: virar genero em portugues e decisao editorial, tomada
/// no painel de quem exibe a biblioteca, com alguem olhando.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Theme {
    pub key: String,
    pub value: String,
}

/// Numeros da plataforma de origem.
///
/// Ficam num bloco proprio porque quem exibe a biblioteca tem os proprios
/// contadores de audiencia: misturar os dois corromperia a analise de la.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Upstream {
    pub num_watched: u64,
    pub play_count: u64,
    pub digg_count: u64,
    pub collect_count: u64,
    pub comment_count: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Series {
    pub drama_id: String,
    pub name: String,
    pub description: String,
    /// Espelhos da capa vertical, em resolucao original.
    ///
    /// Uma lista e nao uma URL porque o TikTok entrega tres CDNs da mesma
    /// imagem — o mesmo padrao dos `Gear`. Todas sao assinadas e expiram: elas
    /// servem para baixar o arquivo da capa, nunca para ser guardadas.
    #[serde(default)]
    pub covers: Vec<String>,
    pub total: u32,
    pub episodes: Vec<Episode>,
    #[serde(default)]
    pub source: Source,
    /// Idioma da pagina de origem. So o ReelShort usa, para remontar as URLs
    /// dos episodios na hora de baixar.
    #[serde(default)]
    pub lang: String,

    #[serde(default)]
    pub themes: Vec<Theme>,
    /// Duracao da serie inteira, em segundos, como a origem declara.
    #[serde(default)]
    pub total_duration_sec: u32,
    #[serde(default)]
    pub upstream: Upstream,
    /// Promocao de "gratis por tempo limitado" na origem. So o TikTok declara.
    #[serde(default)]
    pub is_limited_free: bool,
}

impl Series {
    /// URL canonica da serie, de onde ela veio.
    ///
    /// E o unico dado que devolve a identidade de uma pasta depois que tudo
    /// mais se perde: o nome da pasta nao carrega o id, o `.mp4` do TikTok so
    /// guarda o `videoID` do CDN (que nao tem caminho de volta), e a fila e a
    /// lista de acompanhadas somem quando o usuario as limpa. Por isso ela e
    /// gravada no manifesto, dentro da propria pasta.
    ///
    /// Derivada, e nao guardada, para nao existirem duas verdades: o que o
    /// usuario colou pode ser o link de um episodio ou de um video solto.
    pub fn url(&self) -> String {
        series_url(self.source, &self.drama_id, &self.lang)
    }

    /// Extensao do arquivo final.
    ///
    /// As duas fontes terminam em MP4. No ReelShort o MPEG-TS baixado fica como
    /// intermediario recuperavel e e remuxado sem perda antes da publicacao.
    pub fn extension(&self) -> &'static str {
        "mp4"
    }
}

/// Escolhe a melhor variante: respeita o codec pedido e, dentro dele, o maior
/// bitrate — que e o que carrega detalhe de verdade, nao a maior resolucao.
pub fn pick_gear<'a>(ep: &'a Episode, codec_pref: &str, forced: Option<&str>) -> Option<&'a Gear> {
    if ep.gears.is_empty() {
        return None;
    }
    if let Some(name) = forced {
        if let Some(g) = ep.gears.iter().find(|g| g.gear_name == name) {
            return Some(g);
        }
    }
    let preferred: Vec<&Gear> = ep
        .gears
        .iter()
        .filter(|g| g.codec.starts_with(codec_pref))
        .collect();
    let pool: Vec<&Gear> = if preferred.is_empty() {
        ep.gears.iter().collect()
    } else {
        preferred
    };
    pool.into_iter().max_by_key(|g| g.bitrate)
}

/// URL canonica de uma serie, a partir do que a identifica.
///
/// Uma definicao so, usada por quem grava o manifesto e por quem le a pasta de
/// volta. O `lang` importa apenas no ReelShort, e ali ele importa de verdade:
/// a pagina `/en/` e a `/pt/` sao paginas diferentes, e consultar a errada traz
/// a novela no idioma errado sem erro nenhum.
pub fn series_url(source: Source, drama_id: &str, lang: &str) -> String {
    match source {
        Source::Tiktok => format!("https://www.tiktok.com/shortdrama/episode/{drama_id}/1"),
        Source::Reelshort => {
            let lang = if lang.is_empty() { "pt" } else { lang };
            format!("https://www.reelshort.com/{lang}/movie/{drama_id}")
        }
    }
}

/// Pasta da novela dentro da biblioteca.
///
/// Uma unica definicao para todo mundo: a fila grava os episodios aqui, o
/// manifesto e a capa moram aqui, e a varredura que reconstroi os metadados do
/// que ja foi baixado procura aqui. Duas formulas iguais mas separadas era o
/// jeito de uma delas desviar da outra sem ninguem notar.
pub fn series_dir(dest_dir: &str, series_name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(dest_dir).join(safe_name(series_name))
}

/// Nome de arquivo aceitavel no Windows.
pub fn safe_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            c if (c as u32) < 32 => ' ',
            c => c,
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        "drama".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gear(name: &str, codec: &str, bitrate: u64) -> Gear {
        Gear {
            gear_name: name.into(),
            codec: codec.into(),
            width: 0,
            height: 0,
            bitrate,
            size: 0,
            urls: vec!["http://x".into()],
        }
    }

    fn ep(gears: Vec<Gear>) -> Episode {
        Episode {
            number: 1,
            item_id: "i".into(),
            duration: 60,
            gears,
            locked: false,
            lock_reason: String::new(),
            thumb: String::new(),
            created_at: 0,
            is_preview: false,
            is_free_intro: false,
        }
    }

    #[test]
    fn trancado_nunca_esta_disponivel() {
        let mut e = ep(vec![gear("a", "h264", 100)]);
        assert!(e.available());
        // Mesmo com midia alcancavel, o cadeado manda.
        e.locked = true;
        assert!(!e.available());
    }

    #[test]
    fn prefixo_de_fonte_evita_colisao_de_id() {
        assert_ne!(Source::Tiktok.tag(), Source::Reelshort.tag());
    }

    #[test]
    fn nome_de_arquivo_seguro() {
        assert_eq!(safe_name("a/b:c?"), "a b c");
        assert_eq!(safe_name("   "), "drama");
    }
}
