//! Impede o Windows de suspender enquanto a fila tem trabalho.
//!
//! Este modulo existe por causa de um defeito que nenhuma logica de
//! retentativa alcanca: a cena real do app e "mando baixar cinquenta novelas e
//! vou dormir". Se a MAQUINA dorme junto, todo download para no meio — e o
//! usuario acorda achando que o app falhou, quando o app nunca chegou a
//! rodar. Fila imortal em maquina suspensa nao baixa nada.
//!
//! Duas decisoes:
//!
//! - **So o sistema, nunca a tela.** `ES_DISPLAY_REQUIRED` deixaria o monitor
//!   aceso a noite inteira, o que e uma imposicao — e nao tem nada a ver com
//!   baixar arquivo. O pedido e apenas "nao suspenda".
//!
//! - **O pedido acompanha a fila, e nao a vida do app.** Fica ligado enquanto
//!   ha algo baixando ou esperando, e e devolvido assim que a fila esvazia. Um
//!   app aberto e ocioso nao tem direito nenhum de manter o computador ligado.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
    };

    /// O que ja foi pedido, para nao repetir a chamada a cada tique.
    static SEGURANDO: AtomicBool = AtomicBool::new(false);

    /// `ativo` liga o pedido; `false` devolve o controle ao Windows.
    ///
    /// O estado e por THREAD, entao toda chamada tem de sair da mesma — no app
    /// e sempre o laco do agendador.
    pub fn manter_acordado(ativo: bool) {
        if SEGURANDO.swap(ativo, Ordering::Relaxed) == ativo {
            return;
        }
        // ES_CONTINUOUS sozinho = "esqueca o que eu pedi".
        let flags = if ativo {
            ES_CONTINUOUS | ES_SYSTEM_REQUIRED
        } else {
            ES_CONTINUOUS
        };
        // Sem `unsafe` nao ha como falar com a API do sistema. A chamada nao
        // tem efeito colateral alem do proprio pedido, e um erro dela so
        // significa que a maquina pode suspender — nao que algo quebrou.
        unsafe {
            SetThreadExecutionState(flags);
        }
    }

}

#[cfg(not(windows))]
mod imp {
    pub fn manter_acordado(_ativo: bool) {}
}

pub use imp::manter_acordado;
