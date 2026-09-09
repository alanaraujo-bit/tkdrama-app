# tkdrama

## O que é

App de desktop para Windows que baixa novelas verticais (short dramas) do TikTok
e guarda em disco para assistir offline, em qualquer player, a qualquer momento.

## Mecanismo único

O TikTok não oferece download dessas séries e não expõe a lista de episódios como
playlist. O app descobre os episódios sozinho pela API interna usando a sessão do
próprio usuário, e baixa tudo em fila — o que hoje só existe como trabalho manual,
episódio por episódio.

## Quem usa

Um usuário único (Alan), no Windows 11, na própria máquina. Não é multiusuário,
não é servidor, não tem conta nem sincronização.

## A cena real

À noite, luz baixa. Ele cola o link de uma novela, manda baixar a série inteira,
e volta depois para conferir. O app fica aberto em segundo plano enquanto a fila
corre. Ao abrir, a primeira pergunta que ele faz é "como está a fila?".

Com o tempo a cena muda: as novelas que ele acompanha se reabastecem sozinhas.
Ele abre o app e os episódios que saíram desde ontem já estão baixados.

## O trabalho principal

1. Entrar no TikTok uma vez (janela de login dentro do app).
2. Colar o link da novela — um, ou **muitos de uma vez**. Apontar novela a
   novela é o gargalo real quando a lista tem dezenas: colar trinta links de
   uma vez e sair é o caminho que mantém a promessa de "durmo e acordo com
   tudo baixado".
3. **Baixar tudo** — o caminho dominante. Escolher episódios soltos existe, mas é
   a exceção.
4. **Acompanhar** a novela — aponta-se uma vez e o app revisita sozinho, de
   tempos em tempos, enfileirando o episódio novo assim que ele existe. É o que
   dispensa voltar ao TikTok a cada semana para colar o mesmo link de novo.
5. Acompanhar a fila; encontrar os arquivos na pasta.

## Estados que importam

- Sem login / sessão expirada (o app precisa dizer isso sem jargão)
- Buscando a série
- Fila: na fila, baixando, pausado, concluído, falhou, bloqueado
- Episódio bloqueado (exige desbloqueio no TikTok — o app pula e avisa)
- Espaço em disco insuficiente para o que está na fila
- Fila vazia (primeiro uso)
- Novela acompanhada: nunca verificada, verificada há X, pausada, com erro na
  última rodada (uma que falha não pode derrubar as outras)
- Rodada automática interrompida por um teto (séries, GB) ou pelo piso de disco

## Verdades técnicas que a interface precisa respeitar

- **Qualidade**: maior resolução não é melhor qualidade. O 1080p desse catálogo é
  upscale com bitrate menor que o 540p. O app escolhe por bitrate e precisa
  explicar isso onde o usuário escolhe qualidade, senão a escolha parece errada.
- **Links expiram**: as URLs de mídia são assinadas e vencem. Retomar um download
  antigo exige reconsultar a API — isso pode causar uma pausa antes do progresso
  voltar a andar.
- **Retomada é real**: downloads sobrevivem a fechar o app, queda de rede e
  desligamento. Nada é rebaixado do zero.
- **Episódios são curtos**: 60-200s, 3-18 MB. Uma série inteira tem ~40 episódios
  e ~380 MB. A fila é de muitos itens pequenos, não de poucos itens grandes —
  linhas de progresso passam rápido.

## Limites deliberados

- Baixa só o que a conta do usuário já pode assistir. Não contorna DRM (não há),
  nem paywall, nem bloqueio por moeda. Isto vale igualmente para o que entra
  sozinho: a rodada automática usa o mesmo caminho de direito, e episódio fora
  do alcance da conta entra na lista como bloqueado, nunca como download.
- Sem player embutido: o objetivo é ter o arquivo, assistido onde o usuário quiser.
- **O app nunca escolhe novela sozinho.** Este é o limite, e ele é sobre
  *decisão*, não sobre *informação*: o app pode mostrar o que existe, mas nada
  entra na pasta sem o usuário ter marcado. Colar trinta links de uma vez não
  fere isso — continua sendo ele apontando, em lote em vez de um a um. A aba
  *Descobrir* também não: ela lê a vitrine da plataforma e propõe uma lista com
  caixinhas; quem marca é o usuário.

  O que caiu foi o "sem catálogo dentro do app". Ele existia por tabela — era
  a forma mais fácil de garantir a regra de cima — mas custava caro: para saber
  se uma novela já estava baixada, era preciso sair do app, abrir a plataforma
  e voltar com um link. Mostrar a vitrine devolve essa resposta sem entregar a
  decisão.

  A vitrine é **curada e rotativa**, não o acervo: é o que a plataforma destaca
  hoje. A interface precisa dizer isso, senão "105 novelas" se lê como "tudo o
  que falta" — que seria mentira.
- A pasta só contém novela que o usuário apontou. É por isso que o automático
  serve a lista de acompanhadas, e não um ranking: um agendador sobre "as mais
  em alta" encheria o disco de série que ninguém escolheu assistir.
- O que entra sozinho tem prioridade baixa: nunca passa na frente de um download
  pedido na mão.

## Compromissos de marca

Nenhum. Produto pessoal, sem identidade herdada, sem logo, sem cores obrigatórias.
