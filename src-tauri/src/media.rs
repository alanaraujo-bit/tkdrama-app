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
}

impl Episode {
    /// Da para baixar? Trancado nunca, mesmo que a midia esteja alcancavel.
    pub fn available(&self) -> bool {
        !self.locked && !self.gears.is_empty()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Series {
    pub drama_id: String,
    pub name: String,
    pub description: String,
    pub cover: String,
    pub total: u32,
    pub episodes: Vec<Episode>,
    #[serde(default)]
    pub source: Source,
    /// Idioma da pagina de origem. So o ReelShort usa, para remontar as URLs
    /// dos episodios na hora de baixar.
    #[serde(default)]
    pub lang: String,
}

impl Series {
    /// Extensao do arquivo final.
    ///
    /// O ReelShort so entrega HLS: os segmentos sao MPEG-TS e sao emendados sem
    /// reempacotar, entao o arquivo e um `.ts` de verdade. Chamar de `.mp4` o
    /// que nao e mp4 quebraria players que confiam na extensao.
    pub fn extension(&self) -> &'static str {
        match self.source {
            Source::Tiktok => "mp4",
            Source::Reelshort => "ts",
        }
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
