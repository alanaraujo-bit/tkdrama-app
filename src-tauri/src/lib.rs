//! tkdrama - baixa novelas verticais do TikTok.
//!
//! Seguranca: a janela "login" carrega tiktok.com e por isso NAO recebe nenhuma
//! permissao do app (ver capabilities/default.json, `windows: ["main"]`).
//! Conteudo externo nunca enxerga as APIs nativas.

mod awake;
mod backfill;
mod bulk;
pub mod candyjar;
pub mod dramabox;
mod disk;
pub mod manifest;
pub mod media;
pub mod download;
mod queue;
pub mod reelshort;
mod tiktok;
mod watch;

use std::sync::Arc;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use queue::{AppState, Settings, Status, Task};

type Res<T> = Result<T, String>;

// ------------------------------------------------------------------ autenticacao

/// Monta o header Cookie a partir dos cookies do TikTok guardados no WebView2.
///
/// O perfil do WebView2 e proprio do app e persiste entre execucoes, entao o
/// login sobrevive ao fechamento.
/// Le os cookies na thread principal e devolve o resultado.
///
/// Os objetos do WebView2 sao apartment-threaded: chamar `cookies()` de uma
/// thread do pool async trava. Todo acesso passa por aqui.
async fn collect_cookie(app: &AppHandle, source: media::Source) -> (String, bool) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = app.clone();
    if app
        .run_on_main_thread(move || {
            let _ = tx.send(collect_cookie_blocking(&handle, source));
        })
        .is_err()
    {
        return (String::new(), false);
    }
    rx.await.unwrap_or_else(|_| (String::new(), false))
}

fn collect_cookie_blocking(app: &AppHandle, source: media::Source) -> (String, bool) {
    let mut pairs: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut has_session = false;
    let domain = source.cookie_domain();

    for label in [source.login_window(), "main"] {
        let Some(w) = app.get_webview_window(label) else {
            continue;
        };
        let Ok(cookies) = w.cookies() else { continue };
        for c in &cookies {
            if !c.domain().unwrap_or("").contains(domain) {
                continue;
            }
            let name = c.name().to_string();
            // Cookie interno de diagnostico (canal da sonda): nao e do site.
            if name == "tkpayload" {
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            if Some(name.as_str()) == source.session_cookie() {
                has_session = true;
            }
            pairs.push(format!("{}={}", name, c.value()));
        }
        if !pairs.is_empty() {
            break;
        }
    }
    // Sem cookie de sessao conhecido (caso do ReelShort), a unica coisa
    // honesta e dizer "tem cookies", nao afirmar que a conta esta logada.
    if source.session_cookie().is_none() {
        has_session = !pairs.is_empty();
    }
    (pairs.join("; "), has_session)
}

async fn refresh_cookie(app: &AppHandle, source: media::Source) -> bool {
    let (cookie, has_session) = collect_cookie(app, source).await;
    let state = app.state::<Arc<AppState>>();
    state.cookies.lock().unwrap().insert(source, cookie);
    has_session
}

#[tauri::command]
async fn auth_status(app: AppHandle) -> Res<serde_json::Value> {
    let mut out = serde_json::Map::new();
    for source in media::SOURCES {
        let logged = refresh_cookie(&app, source).await;
        let state = app.state::<Arc<AppState>>();
        let count = state
            .cookie_for(source)
            .split("; ")
            .filter(|s| !s.is_empty())
            .count();
        out.insert(
            source.tag().to_string(),
            serde_json::json!({
                "logged": logged,
                "cookies": count,
                // Sem cookie de sessao conhecido nao da para afirmar login;
                // a interface usa isto para nao mentir na etiqueta.
                "certain": source.session_cookie().is_some(),
                "label": source.label(),
            }),
        );
    }
    Ok(serde_json::Value::Object(out))
}

/// Grampeia fetch/XHR na janela do ReelShort para descobrir QUAL endpoint diz o
/// que a conta liberou.
///
/// A pagina do episodio nao carrega esse dado (a sonda confirmou: resposta
/// identica para logado e anonimo). Mas o perfil mostra "Episodes Unlocked", e
/// essa aba busca a informacao em algum lugar. Em vez de adivinhar URL, deixa a
/// propria pagina revelar.
///
/// Canal de retorno: o mesmo do resolvedor do TikTok — a URL vai para o
/// `document.title` e o Rust le por polling. A janela continua sem nenhuma
/// permissao do app; nao ha IPC.
const RS_SNIFFER: &str = r#"
  (function () {
    if (window.__tkrs) return;
    window.__tkrs = true;
    var vistos = [];

    // Estatico nao interessa: so chamadas de dados.
    function ignorar(u) {
      return /\.(js|css|png|jpe?g|webp|gif|svg|woff2?|ttf|ico|mp4|m3u8|ts)(\?|$)/i.test(u)
          || /google|gstatic|facebook|clarity|sentry|analytics|doubleclick/i.test(u);
    }
    function anota(u) {
      if (!u) return;
      u = String(u);
      try { u = new URL(u, location.href).href; } catch (e) {}
      if (ignorar(u) || vistos.indexOf(u) !== -1) return;
      vistos.push(u);
      pintar();
    }
    // --- credencial da sessao -------------------------------------------
    // Cookies sozinhos chegam como visitante: a API do ReelShort autentica por
    // um token que o site guarda e manda em cabecalho. Aqui ele e capturado da
    // SESSAO DO PROPRIO USUARIO e entregue ao app pelo titulo da janela — nunca
    // e desenhado na tela, para nao vazar num print.
    var cabecalhos = {};   // nome -> valor (credencial; nao exibir)
    var nomesVistos = [];  // so os nomes; esses sim aparecem na caixa
    var chavesLocais = [];

    // Modelos de requisicao aprendidos ao vivo. Endpoints como
    // getChapterContent sao POST e nao da para adivinhar o corpo: aqui ele e
    // aprendido observando a propria pagina chamar, e depois reaproveitado
    // trocando so o capitulo.
    var modelos = {};
    function guardaCorpo(url, metodo, corpo) {
      try {
        if (!url || corpo == null) return;
        url = String(url);
        var i = url.indexOf('/api/video/');
        if (i === -1) return;
        var nome = url.slice(i + 11).split('?')[0];
        // O corpo nem sempre chega como texto: pode ser FormData, URLSearchParams
        // ou um stream. Normalizar aqui e o que permite reaproveita-lo depois.
        if (typeof corpo !== 'string') {
          if (corpo instanceof URLSearchParams) corpo = corpo.toString();
          else if (typeof FormData !== 'undefined' && corpo instanceof FormData) {
            var p = []; corpo.forEach(function (v, k) { p.push(k + '=' + encodeURIComponent(v)); });
            corpo = p.join('&');
          } else { try { corpo = JSON.stringify(corpo); } catch (e) { return; } }
        }
        if (!corpo) return;
        modelos[nome] = { metodo: metodo || 'POST', corpo: corpo };
        pintar();
      } catch (e) {}
    }

    // "CommonParam Err": a API exige um bloco de parametros comuns (versao,
    // plataforma, idioma...) que o interceptador do site anexa a todo POST.
    // Eles sao os MESMOS em todos os endpoints, entao basta aprende-los de
    // qualquer chamada que a pagina ja tenha feito.
    var ESPECIFICOS = /^(chapter_id|book_id|page|page_size|id|serial_number)$/;
    function baseComum() {
      for (var nome in modelos) {
        var m = modelos[nome];
        if (!m || !m.corpo || m.corpo.charAt(0) !== '{') continue;
        try {
          var o = JSON.parse(m.corpo);
          var base = {};
          var achou = false;
          for (var k in o) {
            if (ESPECIFICOS.test(k)) continue;
            base[k] = o[k];
            achou = true;
          }
          if (achou) return base;
        } catch (e) {}
      }
      return null;
    }

    // Guarda TODO cabecalho que o site anexa as chamadas da API.
    //
    // Antes havia aqui uma lista fechada (session, uid, token...) e ela
    // descartava justamente os parametros comuns — versao do app, plataforma,
    // idioma — que o servidor cobra com "CommonParam Err". O corpo que o site
    // envia em getChapterContent e so {book_id, chapter_id}, identico ao meu:
    // logo, o que falta viaja em cabecalho. Por isso agora se copia tudo,
    // menos o que o proprio navegador controla.
    var PROIBIDOS = /^(content-length|host|origin|referer|cookie|user-agent|accept-encoding|connection|content-type)$/i;
    function guardaCabecalho(nome, valor) {
      if (!nome || !valor) return;
      var n = String(nome);
      if (PROIBIDOS.test(n)) return;
      cabecalhos[n] = String(valor);
      if (nomesVistos.indexOf(n) === -1) { nomesVistos.push(n); pintar(); }
    }
    function varreLocalStorage() {
      try {
        for (var i = 0; i < localStorage.length; i++) {
          var k = localStorage.key(i);
          if (!/token|auth|jwt|uid|user|session|login/i.test(k)) continue;
          var v = localStorage.getItem(k);
          if (!v || v.length > 4000) continue;
          if (chavesLocais.indexOf(k) === -1) { chavesLocais.push(k); }
          cabecalhos['LS:' + k] = v;
        }
      } catch (e) {}
    }

    try {
      var of = window.fetch;
      window.fetch = function (a, b) {
        var alvo = typeof a === 'string' ? a : (a && a.url);
        try { anota(alvo); } catch (e) {}
        try { guardaCorpo(alvo, (b && b.method) || 'GET', b && b.body); } catch (e) {}
        try {
          var h = (b && b.headers) || (a && a.headers);
          if (h) {
            if (typeof h.forEach === 'function') h.forEach(function (v, k) { guardaCabecalho(k, v); });
            else Object.keys(h).forEach(function (k) { guardaCabecalho(k, h[k]); });
          }
        } catch (e) {}
        return of.apply(this, arguments);
      };
    } catch (e) {}
    try {
      var oo = XMLHttpRequest.prototype.open;
      XMLHttpRequest.prototype.open = function (m, u) {
        try { this.__tkM = m; this.__tkU = u; anota(u); } catch (e) {}
        return oo.apply(this, arguments);
      };
      var oe = XMLHttpRequest.prototype.send;
      XMLHttpRequest.prototype.send = function (b) {
        try { guardaCorpo(this.__tkU, this.__tkM, b); } catch (e) {}
        return oe.apply(this, arguments);
      };
      var os = XMLHttpRequest.prototype.setRequestHeader;
      XMLHttpRequest.prototype.setRequestHeader = function (k, v) {
        try { guardaCabecalho(k, v); } catch (e) {}
        return os.apply(this, arguments);
      };
    } catch (e) {}

    // --- consulta de direito, feita PELA PROPRIA PAGINA -------------------
    // Copiar a credencial para fora se mostrou fragil: o site pode assinar a
    // requisicao no proprio JS, e ai nenhum cabecalho copiado resolve. Aqui a
    // pergunta e feita de dentro da sessao logada, que ja esta autenticada por
    // construcao. O app so recebe a RESPOSTA: a lista de episodios liberados.
    // As chamadas feitas aqui usam `fetch` puro e NAO passam pelo interceptador
    // do site — que e quem anexa `session`/`uid`. Sem esses cabecalhos o
    // servidor responde code=101 (ERROR), que foi o que travou as tentativas
    // anteriores. Aqui eles sao anexados a mao, vindos da propria sessao.
    function credenciais() {
      try {
        // localStorage e a fonte mais confiavel: existe antes de qualquer
        // chamada de API acontecer.
        ['session', 'uid'].forEach(function (k) {
          var v = localStorage.getItem(k);
          if (v && !cabecalhos[k]) {
            cabecalhos[k] = v.replace(/^"|"$/g, '');
            if (nomesVistos.indexOf(k) === -1) nomesVistos.push(k);
          }
        });
      } catch (e) {}
      var h = {};
      for (var k in cabecalhos) {
        if (k.indexOf('LS:') === 0) continue;
        h[k] = cabecalhos[k];
      }
      return h;
    }
    function comTipo(tipo) {
      var h = credenciais();
      h['Content-Type'] = tipo;
      return h;
    }

    window.__tkEnt = null;
    window.__tkPedirDireito = function (bookId, nonce) {
      window.__tkNonce = nonce;
      window.__tkEnt = 'BUSCANDO';
      var liberados = [];   // com play_info: desbloqueio por moeda
      var todos = [];       // todos os episodios da serie
      var trancado = null;  // um capitulo sem play_info, para testar o VIP
      var pagina = 1;

      // VIP e verificado ANTES de qualquer paginacao. O estado do site ja
      // carregou o perfil da conta no localStorage, entao isto e instantaneo —
      // e resolve a corrida de tempo que fazia a busca ler a lista antiga.
      // Ativo o VIP, a resposta e 'VIP' e o app libera tudo que encontrou, sem
      // depender de enumerar episodio a episodio aqui dentro.
      var vipAgora = vipDoEstado();
      if (vipAgora) {
        window.__tkDiag = 'VIP ativo: ' + vipAgora + ' -> libera tudo';
        window.__tkEnt = 'VIP';
        pintar();
        return;
      }

      // Um POST com o corpo aprendido da propria pagina, ou o palpite padrao.
      function postar(endpoint, extra) {
        var m = modelos[endpoint];
        var corpo, tipo;
        if (m && m.corpo && m.corpo.charAt(0) === '{') {
          // Melhor caso: a propria pagina ja chamou este endpoint. Reaproveita
          // o corpo inteiro, trocando so o capitulo.
          try {
            var o = JSON.parse(m.corpo);
            for (var k in (extra || {})) o[k] = extra[k];
            corpo = JSON.stringify(o);
          } catch (e) { corpo = m.corpo; }
          tipo = 'application/json';
        } else if (m && m.corpo) {
          corpo = m.corpo;
          for (var k2 in (extra || {})) {
            var re = new RegExp('(^|&)' + k2 + '=[^&]*');
            corpo = re.test(corpo) ? corpo.replace(re, '$1' + k2 + '=' + extra[k2])
                                   : corpo + '&' + k2 + '=' + extra[k2];
          }
          tipo = 'application/x-www-form-urlencoded';
        } else {
          // Endpoint ainda nao observado: monta com os parametros comuns
          // aprendidos de outra chamada + os especificos deste pedido.
          var base = baseComum() || {};
          for (var k3 in (extra || {})) base[k3] = extra[k3];
          corpo = JSON.stringify(base);
          tipo = 'application/json';
        }
        return fetch('/api/video/' + endpoint, {
          method: 'POST', credentials: 'include',
          headers: comTipo(tipo), body: corpo
        }).then(function (r) { return r.text(); });
      }
      // Alias legivel: postarRaw devolve o texto cru da resposta.
      var postarRaw = postar;

      // Procura marca de assinatura ativa em qualquer nivel da resposta.
      function achaVip(obj, prof) {
        if (!obj || typeof obj !== 'object' || (prof || 0) > 4) return null;
        for (var k in obj) {
          var v = obj[k];
          if (v && typeof v === 'object') {
            var dentro = achaVip(v, (prof || 0) + 1);
            if (dentro) return dentro;
            continue;
          }
          if (!/vip|subscri|member|premium/i.test(k)) continue;
          // Verdadeiro, numero != 0, ou data de expiracao no futuro.
          var ativo = v === true || v === 1 || v === '1' ||
                      (typeof v === 'number' && v > 1000000000 && v * 1000 > Date.now()) ||
                      (typeof v === 'number' && v > 0 && v < 1000000000);
          if (ativo) return k + '=' + v;
        }
        return null;
      }

      // POST que devolve texto (nunca .json() direto: uma resposta comprimida
      // ou vazia quebrava a leitura e mascarava o resultado real).
      function postarTxt(endpoint, extra) {
        return postarRaw(endpoint, extra).then(function (t) {
          var j = null; try { j = JSON.parse(t); } catch (e) {}
          return { texto: t, json: j };
        });
      }

      // Verdade central deste passo: `ts` e `sign` sao POR-REQUISICAO — o site
      // assina os parametros daquela chamada. Reaproveitar o sign de outra
      // chamada e recusado. Entao a decisao de VIP nao remonta requisicao
      // nenhuma: le direto os campos de assinatura que a PROPRIA pagina ja
      // buscou (getUserInfo roda sozinho no carregamento) do estado do site.
      function vipDoEstado() {
        try {
          var u = JSON.parse(localStorage.getItem('userInfo') || '{}');
          var m = achaVip(u);
          if (m) return m;
        } catch (e) {}
        // Alguns builds guardam em stores globais; varre superficial.
        try {
          for (var k in window) {
            if (!/user|vip|member|store|state/i.test(k)) continue;
            var v = window[k];
            if (v && typeof v === 'object') { var mm = achaVip(v); if (mm) return mm; }
          }
        } catch (e) {}
        return null;
      }

      function testarVip() {
        // 1) O que o proprio site ja sabe da conta, sem refazer chamada.
        var doEstado = vipDoEstado();
        if (doEstado) {
          window.__tkDiag = 'VIP no estado do site: ' + doEstado;
          window.__tkEnt = 'OK:' + todos.join(','); pintar(); return;
        }
        // 2) Consulta getUserInfo lendo como texto (robusto a compressao).
        postarTxt('user/getUserInfo', {})
          .then(function (r) {
            var j = r.json;
            var marca = j && j.code === 0 ? achaVip(j.data) : null;
            window.__tkDiag = 'getUserInfo code=' + ((j && j.code) + '') +
                              ' | vip: ' + (marca || 'nao') +
                              ' | resp: ' + String(r.texto).slice(0, 60);
            if (marca) { window.__tkEnt = 'OK:' + todos.join(','); pintar(); return; }
            // 3) Ultimo recurso: pedir o conteudo de um episodio trancado.
            if (!trancado) { window.__tkEnt = 'OK:' + liberados.join(','); pintar(); return; }
            postarTxt('book/getChapterContent', { chapter_id: trancado, book_id: bookId })
              .then(function (c) {
                var cj = c.json, d = (cj && cj.data) || {};
                var liberou = cj && cj.code === 0 &&
                  (d.play_info || d.video_url || d.video_model);
                window.__tkDiag += ' || getChapterContent code=' + ((cj && cj.code) + '') +
                                   ' resp: ' + String(c.texto).slice(0, 60);
                window.__tkEnt = liberou ? 'OK:' + todos.join(',')
                                         : 'OK:' + liberados.join(',');
                pintar();
              })
              .catch(function (e) {
                window.__tkDiag += ' || getChapterContent falhou: ' + ((e && e.message) || '?');
                window.__tkEnt = 'OK:' + liberados.join(','); pintar();
              });
          })
          .catch(function (e) {
            window.__tkDiag = 'getUserInfo falhou: ' + ((e && e.message) || '?');
            window.__tkEnt = 'OK:' + liberados.join(','); pintar();
          });
      }

      function proxima() {
        if (pagina > 25) { testarVip(); return; }
        fetch('/api/video/book/getChapterList?book_id=' + bookId +
              '&page=' + pagina + '&page_size=50',
              { credentials: 'include', headers: credenciais() })
          .then(function (r) { return r.json(); })
          .then(function (j) {
            var d = (j && j.data) || {};
            var arr = d.list || d.chapter_list || d.data || [];
            for (var i = 0; i < arr.length; i++) {
              var c = arr[i];
              var n = parseInt(c.serial_number, 10);
              if (!(n > 0)) continue;
              todos.push(n);
              var pi = c.play_info;
              if (pi && String(pi).length > 10) liberados.push(n);
              else if (!trancado) trancado = c.chapter_id;
            }
            if (!arr.length || arr.length < 50) { testarVip(); }
            else { pagina++; proxima(); }
          })
          .catch(function (e) {
            window.__tkEnt = 'ERRO:' + ((e && e.message) || 'falhou');
          });
      }
      proxima();
    };

    // Canal para o app: um COOKIE, nao o titulo. O titulo era atropelado pela
    // propria SPA do ReelShort (ela gerencia document.title), entao o Rust
    // nunca via a resposta. O cookie, ao contrario, o Rust le de forma
    // confiavel — e o mesmo mecanismo ja usado para a autenticacao. O blob
    // carrega credencial + direito, carimbado com o nonce do pedido.
    function escreveCookie() {
      try {
        varreLocalStorage();
        var ent = window.__tkEnt && window.__tkEnt !== 'BUSCANDO'
          ? String(window.__tkEnt) : '';
        var blob = JSON.stringify({
          n: window.__tkNonce || 0,
          ent: ent,
          cred: credenciais()
        });
        // Cabe folgado no limite de ~4KB de um cookie.
        document.cookie = 'tkpayload=' + encodeURIComponent(blob) + ';path=/;SameSite=Lax';
      } catch (e) {}
    }
    setInterval(escreveCookie, 200);

    // A sonda trabalha em silencio: sem sobreposicao na tela. `pintar` continua
    // existindo porque varios pontos o chamam, mas nao desenha nada — a caixa
    // de diagnostico foi so andaime para descobrir o canal do direito.
    function pintar() {}
    var t = setInterval(function () { if (document.body) { pintar(); clearInterval(t); } }, 400);
  })();
"#;

#[tauri::command]
async fn open_login(app: AppHandle, source: Option<media::Source>) -> Res<()> {
    let source = source.unwrap_or(media::Source::Tiktok);
    let label = source.login_window();
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let url = source
        .login_url()
        .parse()
        .map_err(|_| "url invalida".to_string())?;
    let mut builder = WebviewWindowBuilder::new(&app, label, WebviewUrl::External(url))
        .title(format!(
            "Entrar no {} - feche esta janela quando terminar",
            source.label()
        ))
        .inner_size(1000.0, 820.0)
        .center();
    if source == media::Source::Reelshort {
        builder = builder.initialization_script(RS_SNIFFER);
    }
    builder.build().map_err(|e| e.to_string())?;
    Ok(())
}

/// Recolhe da janela logada a credencial que a API do ReelShort exige.
///
/// Cookies sozinhos chegam como visitante — foi o que fez o portao de direito
/// falhar na primeira tentativa. O site autentica por um cabecalho, e a sonda
/// injetada o publica em pedacos no titulo da janela (o mesmo canal do
/// resolvedor do TikTok). A credencial vai do navegador direto para a memoria
/// do app: nunca e desenhada na tela nem gravada em arquivo.
///
/// Devolve apenas os NOMES capturados, para a interface poder dar retorno sem
/// expor valor nenhum.
/// Le uma mensagem que a sonda envia em pedacos pelo titulo da janela.
///
/// A janela do ReelShort nao tem permissao nenhuma do app (de proposito), entao
/// o titulo e o unico canal disponivel — o mesmo usado pelo resolvedor do
/// TikTok. `descartar` permite ignorar um valor intermediario (ex.: "BUSCANDO")
/// e seguir esperando o definitivo.
/// Le o blob TKP que a sonda publica em pedacos pelo titulo da janela, e o
/// devolve parseado quando `pronto` aceita.
///
/// A janela do ReelShort nao tem permissao nenhuma do app (de proposito), entao
/// o titulo e o unico canal disponivel. O blob e um so — `{n, ent, cred}` — e
/// carrega credencial e direito juntos, carimbados com o nonce do pedido. Como
/// a sonda republica o mesmo blob em ciclo, `pronto` decide quando o conteudo
/// ja e o esperado (ex.: nonce correto e direito preenchido).
async fn read_payload<F>(
    win: &tauri::WebviewWindow,
    tentativas: u32,
    pronto: F,
) -> Option<serde_json::Value>
where
    F: Fn(&serde_json::Value) -> bool,
{
    let mut ultimo: Option<serde_json::Value> = None;

    for _ in 0..tentativas {
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        // O blob vem no cookie `tkpayload`, escrito pela sonda. Ler cookie e
        // confiavel — a SPA nao interfere, ao contrario do titulo.
        let Ok(cookies) = win.cookies() else { continue };
        let Some(bruto) = cookies
            .iter()
            .find(|c| c.name() == "tkpayload")
            .map(|c| c.value().to_string())
        else {
            continue;
        };
        // O valor foi encodeURIComponent no JS.
        let decodificado = urldecode(&bruto);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&decodificado) {
            if pronto(&v) {
                return Some(v);
            }
            ultimo = Some(v);
        }
    }
    ultimo
}

/// Decodifica percent-encoding (encodeURIComponent). Simples de proposito: o
/// conteudo e JSON ASCII, e so `%XX` e `+` precisam de tratamento.
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extrai os cabecalhos de credencial de um blob e os grava na sessao.
fn store_credentials(app: &AppHandle, payload: &serde_json::Value) -> Vec<String> {
    let Some(cred) = payload.get("cred").and_then(|c| c.as_object()) else {
        return Vec::new();
    };
    let mut headers = Vec::new();
    let mut nomes = Vec::new();
    for (k, v) in cred {
        let Some(valor) = v.as_str() else { continue };
        nomes.push(k.clone());
        headers.push((k.clone(), valor.to_string()));
    }
    if !headers.is_empty() {
        let state = app.state::<Arc<AppState>>();
        state.rs_session.lock().unwrap().headers = headers;
    }
    nomes
}

/// Resposta da janela sobre o direito da conta.
enum Direito {
    /// VIP ativo: libera a serie inteira.
    Tudo,
    /// Conjunto especifico de episodios liberados (desbloqueio por moeda).
    Alguns(std::collections::HashSet<u32>),
}

/// Pergunta o direito da conta PELA JANELA LOGADA e le a resposta.
///
/// A consulta sai de dentro da sessao do usuario (ja autenticada), e a resposta
/// volta carimbada com um nonce unico deste pedido — assim uma leitura nunca
/// pega o resultado de uma busca anterior. De quebra, a mesma leitura recolhe a
/// credencial (vem no mesmo blob).
async fn ask_window_entitlement(app: &AppHandle, book_id: &str) -> Option<Direito> {
    // O id vai para dentro de uma string JS: aceita so o formato conhecido.
    if book_id.is_empty() || !book_id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let label = media::Source::Reelshort.login_window();
    let win = app.get_webview_window(label)?;

    // Nonce unico do pedido: separa esta busca de qualquer outra e torna a
    // leitura imune a resultado velho. u32 pequeno — sem risco de precisao no
    // numero JS.
    let nonce: u32 = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1)
        % 1_000_000) as u32
        + 1;

    let nonce64 = nonce as u64;
    let quer =
        format!("if(window.__tkPedirDireito) window.__tkPedirDireito('{book_id}',{nonce});");
    let combina = |v: &serde_json::Value| {
        v.get("n").and_then(|n| n.as_u64()) == Some(nonce64)
            && v.get("ent").and_then(|e| e.as_str()).map(|s| !s.is_empty()).unwrap_or(false)
    };

    // Re-dispara ate obter a resposta com ESTE nonce (~16s no total). Cobre o
    // caso de a janela ainda estar recarregando (troca de idioma) e
    // `__tkPedirDireito` nao estar definido no primeiro disparo — sem isso a
    // leitura esperaria um nonce que nunca seria produzido.
    let mut payload = None;
    for _ in 0..8 {
        let _ = win.eval(&quer);
        if let Some(v) = read_payload(&win, 25, combina).await {
            payload = Some(v);
            break;
        }
    }
    let payload = payload?;

    // Aproveita a leitura para gravar a credencial da sessao.
    store_credentials(app, &payload);

    let ent = payload.get("ent").and_then(|e| e.as_str())?;
    if ent == "VIP" {
        return Some(Direito::Tudo);
    }
    let lista = ent.strip_prefix("OK:")?;
    let set: std::collections::HashSet<u32> = lista
        .split(',')
        .filter_map(|s| s.trim().parse::<u32>().ok())
        .collect();
    if set.is_empty() {
        None
    } else {
        Some(Direito::Alguns(set))
    }
}

/// Recolhe so a credencial da sessao (sem pedir direito). Le o blob atual.
async fn harvest_reelshort_auth(app: &AppHandle) -> Vec<String> {
    let label = media::Source::Reelshort.login_window();
    let Some(win) = app.get_webview_window(label) else {
        return Vec::new();
    };
    // Aceita qualquer blob que ja traga credencial.
    let Some(payload) = read_payload(&win, 75, |v| {
        v.get("cred")
            .and_then(|c| c.as_object())
            .map(|o| !o.is_empty())
            .unwrap_or(false)
    })
    .await else {
        return Vec::new();
    };
    store_credentials(app, &payload)
}

#[tauri::command]
async fn close_login(app: AppHandle, source: Option<media::Source>) -> Res<bool> {
    let source = source.unwrap_or(media::Source::Tiktok);
    if let Some(w) = app.get_webview_window(source.login_window()) {
        let logged = refresh_cookie(&app, source).await;
        let _ = w.close();
        return Ok(logged);
    }
    Ok(refresh_cookie(&app, source).await)
}

// ----------------------------------------------------------------------- series

/// Resolve o dramaID a partir de uma URL de video, carregando a pagina na
/// sessao logada (janela oculta). O Python nao consegue: a pagina exige que o
/// JS do TikTok calcule uma assinatura, e so um WebView real roda esse JS.
///
/// Canal de retorno: um script injetado varre a pagina e, ao achar o dramaID,
/// escreve "TKDRAMA_ID:<id>" no document.title. O Rust le o title por polling.
/// Nao ha IPC nem permissao concedida a pagina externa: so leitura do titulo.
async fn resolve_drama_from_video(app: &AppHandle, video_id: &str) -> Res<String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    // Roda ANTES dos scripts da pagina (initialization_script), o que permite
    // grampear fetch/XHR. Quando a pagina busca o painel de episodios, a URL
    // carrega "dramaID=<id>" (verificado no HAR) e nos capturamos dali. As
    // varreduras de DOM/estado sao apenas reforco.
    const PROBE: &str = r#"
      (function () {
        var found = null;
        var sawDrama = false;   // alguma requisicao com "drama" na URL
        var sawAny = 0;         // total de requisicoes observadas
        function grab(s) {
          if (!s) return;
          s = String(s);
          if (s.indexOf('drama') !== -1) sawDrama = true;
          if (found) return;
          var m = s.match(/(?:dramaID["'=:\s]{1,4}|shortdrama\/(?:episode|detail)\/)(\d{6,})/);
          if (m) { found = m[1]; document.title = 'TKDRAMA_ID:' + found; }
        }
        try {
          var of = window.fetch;
          window.fetch = function (a) {
            sawAny++;
            try { grab(typeof a === 'string' ? a : (a && a.url)); } catch (e) {}
            return of.apply(this, arguments);
          };
        } catch (e) {}
        try {
          var oo = XMLHttpRequest.prototype.open;
          XMLHttpRequest.prototype.open = function (m, u) {
            sawAny++;
            try { grab(u); } catch (e) {}
            return oo.apply(this, arguments);
          };
        } catch (e) {}
        var start = Date.now();
        function tick() {
          if (found) return;
          try {
            grab(location.href);
            var a = document.querySelector('a[href*="/shortdrama/"]');
            if (a) grab(a.href);
            if (window.__UNIVERSAL_DATA_FOR_REHYDRATION__) grab(JSON.stringify(window.__UNIVERSAL_DATA_FOR_REHYDRATION__));
            if (window.SIGI_STATE) grab(JSON.stringify(window.SIGI_STATE));
          } catch (e) {}
          if (found) return;
          if (Date.now() - start > 18000) {
            // Diagnostico compacto para o caso de falha.
            document.title = 'TKDRAMA_FAIL:req' + sawAny + (sawDrama ? '.drama' : '.nodrama');
            return;
          }
          setTimeout(tick, 400);
        }
        tick();
      })();
    "#;

    if let Some(w) = app.get_webview_window("resolver") {
        let _ = w.close();
    }

    let url = tiktok::watch_url(video_id)
        .parse::<tauri::Url>()
        .map_err(|_| "url de video invalida".to_string())?;

    let win = WebviewWindowBuilder::new(app, "resolver", WebviewUrl::External(url))
        .title("resolvendo...")
        .inner_size(420.0, 720.0)
        .visible(false)
        .initialization_script(PROBE)
        .build()
        .map_err(|e| e.to_string())?;

    // Aguarda o title mudar; ~20s de teto (folga sobre o timeout de 18s do JS).
    let mut found: Option<String> = None;
    let mut diag = String::new();
    for _ in 0..66 {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        if let Ok(title) = win.title() {
            if let Some(rest) = title.strip_prefix("TKDRAMA_ID:") {
                found = Some(rest.trim().to_string());
                break;
            }
            if let Some(rest) = title.strip_prefix("TKDRAMA_FAIL") {
                diag = rest.trim_start_matches(':').to_string();
                break;
            }
        }
    }
    let _ = win.close();

    found.ok_or_else(|| {
        format!(
            "Nao consegui identificar a novela desse video. Por enquanto, copie o link \
             pela tela da serie no TikTok (a que mostra a grade de episodios). [{}]",
            if diag.is_empty() { "sem resposta" } else { &diag }
        )
    })
}

#[tauri::command]
async fn fetch_series(app: AppHandle, url: String) -> Res<media::Series> {
    let series = load_series(&app, url).await?;

    // A pasta desta novela ja existe na biblioteca? Entao esta busca acabou de
    // revelar a identidade dela, e e a unica hora em que isso acontece: o nome
    // da pasta nao carrega o `dramaID`, e nao ha busca por nome na API.
    //
    // Gravar a ficha aqui e o que resgata o que foi baixado antes de o
    // manifesto existir. Dai em diante a pasta se identifica sozinha — o
    // proprio `manifest.json` responde — e a varredura de metadados nao precisa
    // mais do link.
    let state = app.state::<Arc<AppState>>().inner().clone();
    let dest = state.settings.lock().unwrap().dest_dir.clone();
    if media::series_dir(&dest, &series.name).is_dir() {
        queue::spawn_series_manifest(&state, &series, &dest);
    }
    Ok(series)
}

/// Corpo real do `fetch_series`, reusavel fora do comando.
///
/// O laco de `watch.rs` entra por aqui em vez de falar com os adaptadores
/// direto: e este caminho que carrega a autenticacao das duas fontes — em
/// especial a do ReelShort, que depende de perguntar o direito a janela logada.
pub(crate) async fn load_series(app: &AppHandle, url: String) -> Res<media::Series> {
    // ReelShort primeiro: o reconhecimento dele exige o dominio na URL, entao
    // nunca rouba um link do TikTok.
    if let Some((lang, book_id)) = reelshort::parse_url(&url) {
        // Recolhe a sessao do ReelShort, se houver: logado, o site responde
        // como para a conta do usuario.
        // Recolhe a sessao do ReelShort: cookie + credencial da API. Sem a
        // credencial o portao de direito enxerga so o conteudo gratuito.
        refresh_cookie(app, media::Source::Reelshort).await;
        harvest_reelshort_auth(app).await;
        // A janela logada responde pelo direito da conta; e ela que esta
        // autenticada de verdade.
        // (libera_tudo, conjunto_liberado): VIP ativo libera a serie inteira,
        // sem precisar enumerar aqui.
        let (vip_total, liberados) = match ask_window_entitlement(app, &book_id).await {
            Some(Direito::Tudo) => (true, None),
            Some(Direito::Alguns(set)) => (false, Some(set)),
            None => (false, None),
        };
        let state = app.state::<Arc<AppState>>().inner().clone();
        let sess = state.rs_session();
        let series =
            reelshort::fetch_series(&state.client, &lang, &book_id, &sess, liberados, vip_total)
                .await
                .map_err(|e| e.to_string())?;
        state
            .series_cache
            .lock()
            .unwrap()
            .insert(series.source.cache_key(&series.drama_id), series.clone());
        return Ok(series);
    }

    // CandyJar: o portao de direito e por sessao, mas so por cookie — a
    // pagina e renderizada no servidor lendo os cookies da requisicao, sem o
    // cabecalho extra que o ReelShort exige. Basta a sessao ja capturada.
    if let Some((lang, id)) = candyjar::parse_url(&url) {
        refresh_cookie(app, media::Source::Candyjar).await;
        let state = app.state::<Arc<AppState>>().inner().clone();
        let cookie = state.cookie_for(media::Source::Candyjar);
        let series = candyjar::fetch_series(&state.client, &lang, &id, &cookie)
            .await
            .map_err(|e| e.to_string())?;
        state
            .series_cache
            .lock()
            .unwrap()
            .insert(series.source.cache_key(&series.drama_id), series.clone());
        return Ok(series);
    }

    // DramaBox: mesmo caso do CandyJar — a pagina e renderizada no servidor
    // lendo os cookies da requisicao, entao a sessao ja capturada basta e nao
    // ha cabecalho extra a farejar. Deslogado a origem responde como visitante
    // (so o comeco liberado), o que e resposta valida e nao erro.
    if let Some((lang, id)) = dramabox::parse_url(&url) {
        refresh_cookie(app, media::Source::Dramabox).await;
        let state = app.state::<Arc<AppState>>().inner().clone();
        let cookie = state.cookie_for(media::Source::Dramabox);
        let series = dramabox::fetch_series(&state.client, &lang, &id, &cookie)
            .await
            .map_err(|e| e.to_string())?;
        state
            .series_cache
            .lock()
            .unwrap()
            .insert(series.source.cache_key(&series.drama_id), series.clone());
        return Ok(series);
    }

    // URL direta da serie tem prioridade; senao, resolve a partir do video.
    let drama_id = match tiktok::parse_drama_id(&url) {
        Some(id) => id,
        None => match tiktok::parse_video_id(&url) {
            Some(vid) => resolve_drama_from_video(app, &vid).await?,
            None => {
                return Err("Nao reconheci essa URL. Cole o link de uma novela ou de um \
                            episodio do TikTok, do ReelShort, do CandyJar ou do DramaBox."
                    .to_string())
            }
        },
    };

    refresh_cookie(app, media::Source::Tiktok).await;
    let state = app.state::<Arc<AppState>>().inner().clone();
    let cookie = state.cookie_for(media::Source::Tiktok);
    if cookie.is_empty() {
        return Err(tiktok::NO_SESSION.into());
    }

    let series = tiktok::fetch_series(&state.client, &drama_id, &cookie)
        .await
        .map_err(|e| e.to_string())?;
    state
        .series_cache
        .lock()
        .unwrap()
        .insert(series.source.cache_key(&drama_id), series.clone());
    Ok(series)
}


#[tauri::command]
fn enqueue(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    drama_id: String,
    episodes: Vec<u32>,
    priority: u8,
    sequential: Option<bool>,
    source: Option<media::Source>,
) -> Res<usize> {
    // Sem fonte declarada assume TikTok: e o que versoes antigas da interface
    // enviavam, e continua sendo o caso comum.
    let source = source.unwrap_or(media::Source::Tiktok);
    let series = state
        .series_cache
        .lock()
        .unwrap()
        .get(&source.cache_key(&drama_id))
        .cloned()
        .ok_or_else(|| "Serie nao carregada. Busque a novela de novo.".to_string())?;
    let dest = {
        let mut settings = state.settings.lock().unwrap();
        if let Some(enabled) = sequential {
            settings.sequential = enabled;
        }
        settings.dest_dir.clone()
    };
    let added = queue::enqueue(&state, &series, &episodes, &dest, priority);
    // A pasta acaba de nascer: e a hora de deixar nela a ficha da novela. O
    // laco automatico tem a sua propria chamada — quem enfileira e quem grava,
    // senao a serie reabastecida sozinha nunca ganharia manifesto.
    queue::spawn_series_manifest(&state, &series, &dest);
    queue::save(&app);
    Ok(added)
}

// ------------------------------------------------------- series acompanhadas

/// Passa a acompanhar uma serie ja carregada.
///
/// Recebe `url` porque e ela que o laco vai reusar depois: o ReelShort precisa
/// do idioma da pagina, que nao esta no `drama_id`.
#[tauri::command]
fn follow_add(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    drama_id: String,
    url: String,
    source: Option<media::Source>,
) -> Res<bool> {
    let source = source.unwrap_or(media::Source::Tiktok);
    // Sem URL o acompanhamento nasce morto: toda rodada futura falharia em
    // `load_series` com "nao reconheci essa URL", e o erro so apareceria horas
    // depois. Recusar aqui e o unico momento em que da para dizer isso a tempo.
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("Busque a novela de novo antes de acompanhar.".into());
    }
    let name = state
        .series_cache
        .lock()
        .unwrap()
        .get(&source.cache_key(&drama_id))
        .map(|s| s.name.clone())
        .ok_or_else(|| "Serie nao carregada. Busque a novela de novo.".to_string())?;
    let novo = watch::add(
        &state,
        watch::Follow {
            source,
            drama_id,
            name,
            url,
            added_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            last_check: 0,
            last_added: 0,
            paused: false,
            error: None,
        },
    );
    queue::save(&app);
    Ok(novo)
}

#[tauri::command]
fn follow_list(state: State<'_, Arc<AppState>>) -> Vec<watch::Follow> {
    watch::list(&state)
}

#[tauri::command]
fn follow_remove(app: AppHandle, state: State<'_, Arc<AppState>>, key: String) {
    watch::remove(&state, &key);
    queue::save(&app);
}

#[tauri::command]
fn follow_set_paused(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    key: String,
    paused: bool,
) {
    watch::set_paused(&state, &key, paused);
    queue::save(&app);
}

/// "Verificar agora": roda uma rodada na hora, mesmo com o automatico desligado.
#[tauri::command]
async fn watch_run_now(app: AppHandle) -> Res<watch::Report> {
    Ok(watch::run_once(&app, true).await)
}

// -------------------------------------------------------------------- metadados

/// "Atualizar metadados": percorre as pastas ja baixadas e escreve a ficha de
/// cada novela — `manifest.json`, `poster.jpg` e `thumbs/`.
///
/// Nenhum video e baixado. Sem este comando a melhoria so alcancaria o que for
/// baixado de hoje em diante, e a biblioteca atual ficaria sem arte para sempre.
#[tauri::command]
async fn metadata_refresh(app: AppHandle) -> Res<backfill::Report> {
    let report = backfill::run(&app).await?;
    queue::save(&app);
    Ok(report)
}

// ------------------------------------------------------------------------- fila

#[tauri::command]
fn queue_list(state: State<'_, Arc<AppState>>) -> Vec<Task> {
    state.snapshot()
}

#[tauri::command]
fn task_pause(state: State<'_, Arc<AppState>>, id: String) {
    queue::pause(&state, &id);
}

#[tauri::command]
fn task_resume(state: State<'_, Arc<AppState>>, id: String) {
    queue::resume(&state, &id);
}

#[tauri::command]
fn task_cancel(app: AppHandle, state: State<'_, Arc<AppState>>, id: String, delete_file: bool) {
    queue::cancel(&state, &id, delete_file);
    queue::save(&app);
}

#[tauri::command]
fn task_priority(state: State<'_, Arc<AppState>>, id: String, priority: u8) {
    queue::set_priority(&state, &id, priority);
}

#[tauri::command]
fn queue_pause_all(state: State<'_, Arc<AppState>>) {
    for t in state.snapshot() {
        if matches!(t.status, Status::Queued | Status::Running) {
            queue::pause(&state, &t.id);
        }
    }
}

#[tauri::command]
fn queue_resume_all(state: State<'_, Arc<AppState>>) {
    for t in state.snapshot() {
        if matches!(t.status, Status::Paused | Status::Failed) {
            queue::resume(&state, &t.id);
        }
    }
}

#[tauri::command]
fn queue_clear_finished(app: AppHandle, state: State<'_, Arc<AppState>>) {
    queue::clear_finished(&state);
    queue::save(&app);
}

#[tauri::command]
fn queue_retry_failed(state: State<'_, Arc<AppState>>) {
    queue::retry_failed(&state);
}

// -------------------------------------------------------------------- ajustes

#[tauri::command]
fn settings_get(state: State<'_, Arc<AppState>>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn settings_set(app: AppHandle, state: State<'_, Arc<AppState>>, settings: Settings) -> Res<()> {
    if settings.dest_dir.trim().is_empty() {
        return Err("Escolha uma pasta de destino.".into());
    }
    let mut s = state.settings.lock().unwrap();
    s.dest_dir = settings.dest_dir;
    s.concurrency = settings.concurrency.clamp(1, 3);
    s.codec = if settings.codec == "h265" { "h265" } else { "h264" }.into();
    s.gear = settings.gear.filter(|g| !g.trim().is_empty());
    s.sequential = settings.sequential;
    drop(s);
    state.mark_dirty();
    queue::save(&app);
    Ok(())
}

// ---------------------------------------------------------- o que ja tenho

/// O que a pasta de uma novela ja tem, lido do disco.
///
/// A interface perguntava isso a fila ("tarefas com status concluido"), o que e
/// memoria do app e nao verdade do disco: depois de *Limpar concluidos* o app
/// esquecia tudo e voltava a oferecer o download de episodios que estao ali. O
/// disco nunca esquece.
///
/// Conta so o que esta publicado e valido — um `.parcial` interrompido nao e
/// episodio que se tem.
#[tauri::command]
fn series_on_disk(state: State<'_, Arc<AppState>>, name: String) -> serde_json::Value {
    let dest = state.settings.lock().unwrap().dest_dir.clone();
    let dir = media::series_dir(&dest, &name);
    if !dir.is_dir() {
        return serde_json::json!({ "dir": dir.to_string_lossy(), "exists": false, "have": [] });
    }
    let have: Vec<u32> = manifest::episode_files(&dir)
        .into_iter()
        .filter(|(_, nome)| download::published_mp4(&dir.join(nome)))
        .map(|(n, _)| n)
        .collect();
    serde_json::json!({
        "dir": dir.to_string_lossy(),
        "exists": true,
        "have": have,
    })
}

// ----------------------------------------------------------------- vitrine

/// Normaliza um nome para comparar pasta com titulo.
///
/// A pasta e escrita por `media::safe_name`, que ja troca caractere proibido
/// por espaco; e o titulo pode divergir em acento, pontuacao ou caixa sem ser
/// outra novela. Comparar so o que sobra de letra e digito evita oferecer de
/// novo o que ja esta no disco por causa de uma virgula.
fn chave_de_nome(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter_map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' => Some('a'),
            'é' | 'è' | 'ê' | 'ë' => Some('e'),
            'í' | 'ì' | 'î' | 'ï' => Some('i'),
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => Some('o'),
            'ú' | 'ù' | 'û' | 'ü' => Some('u'),
            'ç' => Some('c'),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .collect()
}

/// Esta pasta e desta novela?
///
/// Igualdade, ou uma comecar com a outra. Nunca "contem em qualquer posicao":
/// um titulo curto como "Ela" esta dentro de "A Novela do Ex", e a vitrine
/// esconderia uma novela que o usuario nao tem, dizendo que ele ja tem — falha
/// silenciosa, que ninguem vai investigar.
///
/// Prefixo cobre o caso real que motivou a tolerancia: a pasta ganhar um sufixo
/// ("Nome da Novela Dublado") ou o titulo vir com um a mais que o nome no disco.
/// E ainda assim so a partir de 6 caracteres, porque prefixo curto volta a ser
/// palpite.
fn mesma_novela(titulo: &str, pasta: &str) -> bool {
    if titulo.is_empty() || pasta.is_empty() {
        return false;
    }
    if titulo == pasta {
        return true;
    }
    let menor = titulo.len().min(pasta.len());
    menor >= 6 && (titulo.starts_with(pasta) || pasta.starts_with(titulo))
}

/// Quantos episodios esta pasta tem, para efeito de EXIBICAO.
///
/// Conta arquivo de video com numero, sem validar cada um: `episode_files` ja
/// descarta `.part`/`.parcial`/`.tmp`, que e o que separa "baixando" de
/// "baixado". Validar de verdade custa ler 4 MB e checar o `moov` por arquivo —
/// aceitavel para UMA novela, proibitivo para a vitrine inteira, que sao 121
/// pastas e milhares de arquivos numa tela que precisa responder rapido.
///
/// A precisao que importa mora onde a decisao e tomada: `queue::enqueue` valida
/// estritamente antes de pular um episodio, entao um arquivo corrompido volta
/// para a fila mesmo tendo sido contado aqui.
fn episodios_na_pasta(dir: &std::path::Path) -> usize {
    manifest::episode_files(dir).len()
}

/// Reconta, no disco, quantos episodios cada titulo ja tem. Sem rede.
///
/// A vitrine envelhece assim que a fila entrega alguma coisa: uma novela que
/// aparecia como "nao tenho" as 22h esta baixada as 3h, e a tela continuaria
/// oferecendo o download dela. Buscar a vitrine de novo resolveria, mas gastaria
/// uma requisicao para responder uma pergunta que o disco responde sozinho.
#[tauri::command]
fn discover_have(state: State<'_, Arc<AppState>>, names: Vec<String>) -> Vec<usize> {
    let dest = state.settings.lock().unwrap().dest_dir.clone();
    let pastas = pastas_da_biblioteca(&dest);
    names
        .iter()
        .map(|n| {
            let chave = chave_de_nome(n);
            pastas
                .iter()
                .find(|(nome, _)| mesma_novela(&chave, nome))
                .map(|(_, dir)| episodios_na_pasta(dir))
                .unwrap_or(0)
        })
        .collect()
}

/// As pastas da biblioteca, ja com a chave de comparacao. Uma leitura so:
/// perguntar por novela faria 121 varreduras para a mesma resposta.
fn pastas_da_biblioteca(dest: &str) -> Vec<(String, std::path::PathBuf)> {
    std::fs::read_dir(dest)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| (chave_de_nome(&e.file_name().to_string_lossy()), e.path()))
        .collect()
}

/// O que a vitrine do ReelShort mostra hoje, marcado com o que voce ja tem.
///
/// Uma requisicao para a lista inteira: nada e consultado por novela aqui. A
/// pergunta desta tela e "o que existe que eu nao tenho?", e ela se responde
/// sem tocar em nenhuma pagina de serie.
///
/// **Nao decide nada.** Devolve uma proposta; quem escolhe continua sendo o
/// usuario, marcando o que quer. E o limite do PRODUCT.md de pe: o app nunca
/// enche a pasta com novela que ninguem escolheu.
#[tauri::command]
async fn discover_reelshort(app: AppHandle) -> Res<Vec<serde_json::Value>> {
    refresh_cookie(&app, media::Source::Reelshort).await;
    harvest_reelshort_auth(&app).await;
    let state = app.state::<Arc<AppState>>().inner().clone();
    let sess = state.rs_session();

    let vitrine = reelshort::browse(&state.client, "pt", &sess)
        .await
        .map_err(|e| e.to_string())?;

    let (dest, seguidas) = {
        let dest = state.settings.lock().unwrap().dest_dir.clone();
        let seguidas: Vec<String> = watch::list(&state)
            .iter()
            .map(|f| f.key())
            .collect();
        (dest, seguidas)
    };

    let pastas = pastas_da_biblioteca(&dest);

    let mut out = Vec::with_capacity(vitrine.len());
    for l in vitrine {
        let chave = chave_de_nome(&l.title);
        let pasta = pastas.iter().find(|(nome, _)| mesma_novela(&chave, nome));
        // Quantos episodios ja existem — nao "a pasta existe".
        let tenho = pasta.map(|(_, dir)| episodios_na_pasta(dir)).unwrap_or(0);
        let seguindo = seguidas
            .iter()
            .any(|k| *k == media::Source::Reelshort.cache_key(&l.book_id));

        out.push(serde_json::json!({
            "book_id": l.book_id,
            "name": l.title,
            "cover": l.cover,
            "description": l.description,
            "episodes": l.episodes,
            "paid_start": l.paid_start,
            "url": l.url("pt"),
            "have_episodes": tenho,
            "following": seguindo,
        }));
    }
    Ok(out)
}

// ------------------------------------------------------------ lote de links

/// Le o texto colado e diz o que da para fazer com ele — sem gastar rede.
///
/// Existe separado do `bulk_add` porque a rodada leva minutos: o usuario
/// precisa ver o que vai acontecer ANTES de mandar rodar, e nao descobrir no
/// fim que metade da lista nao era link de novela.
#[tauri::command]
fn bulk_preview(state: State<'_, Arc<AppState>>, text: String) -> Vec<bulk::Candidate> {
    bulk::preview(&state, &text)
}

/// Processa a lista inteira, no ritmo da plataforma. Devolve o relatorio final.
#[tauri::command]
async fn bulk_add(app: AppHandle, urls: Vec<String>, options: bulk::Options) -> bulk::Report {
    bulk::run(&app, urls, options).await
}

#[tauri::command]
fn bulk_cancel() {
    bulk::cancel();
}

#[tauri::command]
fn bulk_running() -> bool {
    bulk::running()
}

#[tauri::command]
fn disk_info(state: State<'_, Arc<AppState>>, path: Option<String>) -> serde_json::Value {
    let dir = path.unwrap_or_else(|| state.settings.lock().unwrap().dest_dir.clone());
    serde_json::json!({
        "free": disk::free_bytes(&dir),
        "needed": queue::remaining_bytes(&state),
        "path": dir,
    })
}

#[tauri::command]
fn open_path(app: AppHandle, path: String) -> Res<()> {
    use tauri_plugin_opener::OpenerExt;
    let p = std::path::PathBuf::from(&path);
    // Se o arquivo ainda nao existe (download em andamento), abre a pasta.
    let target = if p.is_file() {
        p.parent().map(|d| d.to_path_buf()).unwrap_or(p)
    } else {
        p
    };
    app.opener()
        .open_path(target.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(Arc::new(AppState::new()))
        .setup(|app| {
            let handle = app.handle().clone();
            queue::load(&handle);
            queue::start_scheduler(handle.clone());
            // Os dois lacos que sustentam uma fila deixada sozinha a noite
            // inteira: a sonda reabre o portao quando a internet volta, e o
            // zelador reapresenta a fila ao que falhou por motivo passageiro.
            queue::start_connectivity_probe(handle.clone());
            queue::start_janitor(handle.clone());
            watch::start(handle.clone());

            // Fechar a janela minimiza para a bandeja: o processo e a fila
            // seguem vivos. Um clique no icone restaura a janela.
            let show = MenuItem::with_id(app, "show", "Abrir tkdrama", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show])?;
            let icon = app.default_window_icon().cloned();
            let mut tray = TrayIconBuilder::with_id("downloads")
                .menu(&menu)
                .tooltip("tkdrama — downloads em andamento")
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    if event.id().as_ref() == "show" {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        if let Some(window) = tray.app_handle().get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                });
            if let Some(icon) = icon {
                tray = tray.icon(icon);
            }
            tray.build(app)?;

            // O X apenas esconde a janela; nao encerra downloads. O estado e
            // salvo para tambem sobreviver a uma finalizacao abrupta.
            if let Some(main) = app.get_webview_window("main") {
                let h = handle.clone();
                main.on_window_event(move |e| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = e {
                        api.prevent_close();
                        queue::save(&h);
                        if let Some(window) = h.get_webview_window("main") {
                            let _ = window.hide();
                        }
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            auth_status,
            open_login,
            close_login,
            fetch_series,
            enqueue,
            follow_add,
            follow_list,
            follow_remove,
            follow_set_paused,
            watch_run_now,
            metadata_refresh,
            queue_list,
            task_pause,
            task_resume,
            task_cancel,
            task_priority,
            queue_pause_all,
            queue_resume_all,
            queue_clear_finished,
            queue_retry_failed,
            settings_get,
            settings_set,
            disk_info,
            open_path,
            series_on_disk,
            discover_reelshort,
            discover_have,
            bulk_preview,
            bulk_add,
            bulk_cancel,
            bulk_running,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O pareamento pasta/titulo nunca pode casar por acaso.
    ///
    /// Um falso positivo aqui esconde da vitrine uma novela que o usuario NAO
    /// tem, dizendo que ele ja tem. E silencioso: nao ha erro, nao ha log, e a
    /// novela simplesmente nunca aparece para ser marcada.
    #[test]
    fn so_casa_pasta_com_novela_quando_da_para_afirmar() {
        let k = chave_de_nome;

        // O mesmo nome, com a pontuacao e o acento que o disco perde.
        assert!(mesma_novela(
            &k("Peão Descartado, O Incomparável Rei Dragão"),
            &k("Peão Descartado,O Incomparável Rei Dragão")
        ));
        // Sufixo na pasta: o caso real que justifica tolerar prefixo.
        assert!(mesma_novela(&k("A Sem-Pelo"), &k("A Sem-Pelo Dublado")));

        // O perigo: titulo curto que aparece no MEIO do nome da pasta.
        assert!(
            !mesma_novela(&k("Ela"), &k("A Novela do Ex")),
            "titulo curto no meio do nome nao e a mesma novela"
        );
        assert!(!mesma_novela(&k("Amor"), &k("O Grande Amor Perdido")));
        // Prefixo curto continua sendo palpite.
        assert!(!mesma_novela(&k("Rei"), &k("Rei Dragao")));
        // Novelas diferentes que comecam igual nao se confundem.
        assert!(!mesma_novela(&k("A Sem-Pelo"), &k("Beijada por garras")));
        assert!(!mesma_novela("", &k("Qualquer")));
    }
}
