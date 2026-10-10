# SPEC-02 — Segurança e Anonimato

Status: **Draft** · Fase: 0 · Depende de: SPEC-00, SPEC-01

## 1. Identidade

- No primeiro uso, o cliente gera um **par de chaves Ed25519** e guarda localmente, cifrado em repouso com **DPAPI** (escopo do usuário do Windows) ou, opcionalmente, com uma chave derivada de senha (Argon2id).
- O **MemberId** é o hash da chave pública. É o único "nome" de um participante.
- Sem e-mail, telefone, nome ou qualquer coletor de dado pessoal. Sem servidor de identidade.
- Apelido (nickname) é opcional, local e livre — não é verificado nem único. Serve só pra leitura humana na sala.
- **Chave de transporte ≠ identidade.** O QUIC (iroh) usa outra chave Ed25519: descartável por sessão no cliente, persistente no relay (ela está fixada nos convites). Assim o ponto de encontro e quem observa a rede não ligam a mesma pessoa entre salas pela chave de transporte. O relay de uma sala ainda vê o `MemberId` (§5).

## 2. Papéis

"Host" é ambíguo (processo servidor × dono da sala), então a spec usa três papéis distintos:

| Papel | O que é | Tem chave do grupo? | Pode |
|-------|---------|---------------------|------|
| **Dono** | Membro que criou a sala (identificado pela chave dele) | Sim | Tudo que um membro pode + remover membros, emitir convites, promover outro membro a dono |
| **Membro** | Participante admitido no grupo MLS | Sim | Ler/enviar texto e mídia, admitir quem traz convite válido (ver §3) |
| **Relay** | O processo `--host` (SFU) ou o host interno do x1 | **Não** | Rotear pacotes cifrados, ordenar commits MLS (§4.2), validar o token de roteamento, desconectar um `MemberId` sob pedido assinado do dono |

- No **modo servidor**, o relay é uma máquina à parte e **não é membro**.
- No **modo x1**, o criador acumula **dono + membro + relay** no mesmo processo.
- Remover membro (RF-10) é uma ação do **dono**: ele faz o commit MLS `Remove` (rekey) e envia ao relay um pedido assinado para derrubar a conexão daquele `MemberId`. O relay sozinho não consegue remover ninguém do grupo — só cortar a conexão.

## 3. Convites

Requisito: **cada código é único e de uso único** (RF-03).

### 3.1 Ameaça que o desenho precisa fechar

Se o relay validasse o convite **e** isso bastasse para entrar no grupo MLS, o operador do servidor poderia gerar um convite pra si mesmo, entrar como membro e ler tudo — quebrando o "servidor cego". Por isso o convite tem **dois segredos com destinatários diferentes**: um que o relay vê (só pra rotear) e outro que só os membros conhecem (pra admitir no grupo).

### 3.2 Estrutura

```
lerrichat://join?r=<room_id>&h=<host_addr>&t=<route_token>&s=<invite_secret>&f=<inviter_fpr>&e=<expires_at>
```

Campos binários vão em base64url sem padding.

- `r` — id da sala.
- `h` — como achar o host da sala: a **chave pública de transporte** do host (fixa a conexão; quem não tem a chave privada não completa o handshake), a lista de candidatos (IP:porta ou hostname, `SPEC-03` §4.1) e, opcionalmente, o **ponto de encontro** (relay iroh) com a chave do certificado TLS dele. Serializado com postcard.
- `t` — **token de roteamento** (128 bits). O relay guarda só `hash(t)` e o marca como consumido no primeiro uso. Serve para o relay aceitar a conexão e saber de qual sala ela é. **Não dá acesso ao conteúdo.**
- `s` — **segredo de admissão** (256 bits). **Nunca é enviado ao relay.** Só os membros da sala o conhecem (§3.3).
- `f` — fingerprint da chave de identidade de quem convidou, pra o novato validar quem o admitiu.
- `e` — validade (ex.: expira em 24 h).

### 3.3 Fluxo de admissão

1. O membro que convida gera `t` e `s`. Envia `hash(t)` ao relay e distribui `(id_do_convite, s, e)` aos outros membros como **mensagem de aplicação MLS** (cifrada). Assim, qualquer membro online pode admitir o novato, não só quem convidou.
2. O novato conecta ao relay apresentando `t`. O relay confere `hash(t)`, marca como consumido e encaminha a conexão para a sala. Reuso → recusado aqui.
3. O novato publica seu **KeyPackage** MLS acompanhado de `HMAC(HKDF(s), KeyPackage)`.
4. Um membro online confere o HMAC. Se bate, faz o commit `Add` e envia o `Welcome`. O membro também marca `s` como consumido para todo o grupo (mensagem MLS).
5. O novato valida que o `Welcome` foi assinado por um membro do grupo (e, se for quem convidou, que bate com `f`).

Consequências:

- O relay **nunca** vê `s` nem a chave do grupo. Mesmo emitindo seus próprios `t`, não consegue produzir um HMAC válido, logo não consegue ser admitido.
- **Entrar exige pelo menos um membro online** para admitir. No x1 isso é automático (o criador é o relay).
- Convite expirado ou revogado → recusa antes de qualquer handshake de mídia.

O link é exibido como texto (`lerrichat://...`) e como **QR**. Ele deve trafegar **por fora** (mensageiro, pessoalmente), nunca pelo próprio relay.

### 3.4 Entrar numa conexão e criar salas

- **Prova de identidade por conexão.** O primeiro frame de toda conexão carrega a chave de identidade do membro e uma assinatura Ed25519 sobre `"lerrichat/hello/v1" || chave do host || chave de transporte do cliente`. A assinatura só vale para aquela conexão; quem a captura não a reaproveita.
- **Criar sala no modo servidor** exige o **segredo de administração** do relay, gerado na primeira execução e guardado em disco (DPAPI no Windows, 0600 no Linux). O relay o mostra num link `lerrichat://admin?h=&a=`, que é só de quem opera o servidor. Sem isso, qualquer um que achasse o relay poderia criar salas nele.
- O relay também mostra um link **público** `lerrichat://relay?h=`, que serve de ponto de encontro para salas x1 (`SPEC-03` §4.3). Ele não dá acesso a nada.
- **Voltar a uma sala** não usa convite: o relay confere que o `MemberId` provado já é membro.
- No **x1**, o segredo de administração existe só na memória do app de quem cria.

## 4. Criptografia ponta a ponta (E2EE)

### 4.1 Grupo e texto
- **Grupo:** MLS (RFC 9420), ciphersuite `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519`. Cada sala é um grupo MLS.
- **Entrada/saída:** cada `Add`/`Remove` gera um **novo epoch** com chaves novas. Quem foi removido não deriva as chaves dos epochs seguintes; quem entrou não recebe as chaves dos epochs anteriores.
- **Forward secrecy:** vazar as chaves de hoje não revela mensagens passadas (as chaves antigas são apagadas).
- **Post-compromise security:** depois de um comprometimento, um commit de `Update` volta a deixar o atacante de fora. Os clientes fazem `Update` periódico (ex.: a cada 24 h ou 1000 mensagens).
- **Texto:** cada mensagem é uma mensagem de aplicação MLS do epoch atual.

### 4.2 Delivery Service (ordenação de commits)
MLS exige que todos apliquem os commits na mesma ordem. O **relay é o Delivery Service**: para cada epoch, aceita o **primeiro** commit que chegar, repassa a todos e rejeita os concorrentes (o autor reconstrói sobre o novo epoch e reenvia). O relay ordena sem ler — commits são assinados e o conteúdo sensível vai cifrado.

### 4.2.1 Como ficou na Fase 2
- **Credencial MLS = chave de identidade.** A credencial básica de cada folha é a chave Ed25519 da identidade, e ela mesma assina no MLS. Quem admite confere que a credencial é a chave que assina, e que o `MemberId` dela é quem o relay diz que pediu.
- **Commits** vão como `PublicMessage` (assinados, sem cifra de conteúdo): o relay vê quem entra e sai, o que ele já sabe pelo roteamento. Mensagens de aplicação vão sempre como `PrivateMessage`, com padding a múltiplos de 32 bytes.
- **Log de commits no relay** (até 5000 por sala): quem volta informa o epoch local e recebe o que perdeu, na ordem. Não é mensagem offline: é só o estado do grupo. Quem ficar atrás demais ou corromper o estado pede **readição**, que só é aceita para quem já está no grupo (troca da folha, sem convite).
- **Welcome e pedidos de entrada** ficam guardados no relay até alguém poder tratá-los. Quem pede entrada com convite aguarda até um membro online que conheça `s` aparecer.
- **`s` dos convites** é distribuído pelo dono ao grupo numa mensagem MLS; os membros só aceitam esse compartilhamento vindo do dono.
- **Remoção:** o relay só aceita um commit que remove alguém que não saiu por conta própria se ele vier do dono, e então corta a conexão do removido. Os clientes avisam se um commit de remoção vier de outro membro.
- **Saída:** o membro avisa o relay, que repassa a saída; o primeiro membro online faz o commit de remoção (os outros perdem a disputa pelo epoch, o que é esperado). O dono ainda não pode sair: transferir a posse fica para depois.
- **Update** periódico a cada 12 h de conexão.

### 4.3 Mídia (voz/tela/câmera) — SFrame
- Mídia é cifrada com **SFrame (RFC 9605)**. Cada remetente tem sua própria chave, derivada do grupo pelo **MLS exporter** (`exporter("lerrichat sframe", sender_index, epoch)`). Chave por remetente evita reuso de nonce entre transmissores que compartilham o mesmo grupo.
- O SFU precisa de um **cabeçalho mínimo em claro** pra rotear: id do stream, camada de simulcast, flag de keyframe, número de sequência. Esse cabeçalho é **autenticado** (entra como AAD no AEAD), mas não é sigiloso. Ele revela metadados de mídia, nunca conteúdo.
- O **SFU roteia sem a chave** — não consegue decifrar nem um frame.

### 4.4 Transporte
QUIC já dá TLS 1.3 entre cliente e relay. A E2EE é uma **segunda camada** por cima, pra que nem o relay leia. Defesa em profundidade.

## 5. O relay é cego — o que ele vê e não vê

| O relay **vê** | O relay **não vê** |
|----------------|--------------------|
| Que um `MemberId` conectou | Quem é a pessoa (sem dado pessoal) |
| O **IP** de quem conecta | Conteúdo de texto, voz, tela, câmera |
| Tamanho e tempo dos pacotes (metadados) | A chave do grupo / de mídia |
| A que sala o pacote pertence (pra rotear) | O segredo de admissão `s` dos convites |
| Cabeçalho SFrame: stream, camada, keyframe | Qual membro disse o quê (conteúdo) |

Isso cumpre "servidor sem acesso ao conteúdo" (princípio 2). O que **não** dá pra esconder de quem hospeda é o IP e os metadados de roteamento — ver modelo de ameaça.

## 6. Modelo de ameaça (honesto)

### O que o Lerri Chat protege
- **Conteúdo:** ninguém no caminho (relay incluso) lê mensagens, ouve voz ou vê tela/câmera.
- **Admissão:** o operador do relay não consegue se colocar dentro da sala (§3.1).
- **Anonimato de identidade:** não há cadastro; não dá pra ligar a sala a um e-mail/telefone.
- **Expulsão limpa:** quem é removido perde acesso ao conteúdo futuro (novo epoch MLS).

### O que o Lerri Chat NÃO protege (e o usuário precisa saber)
- **IP visível ao relay e, dependendo do modo, aos peers.** Em modo x1, seu IP é visto pelo outro participante. Em modo servidor, o relay vê o IP de todos.
  - *Mitigação opcional do usuário:* VPN. Esconder IP de forma embutida exigiria Tor, que não sustenta voz/vídeo em tempo real — fora de escopo.
- **Metadados de tráfego** (quem conectou quando, volume, quem está transmitindo vídeo) são visíveis a quem controla o relay/rede.
- **Relay malicioso negando serviço:** ele pode descartar pacotes ou recusar commits. Não lê nada, mas pode derrubar a sala.
- **Link de convite vazado:** quem obtiver o link inteiro (`t` + `s`) antes do convidado entra no lugar dele. O link deve ir por canal confiável.
- **Comprometimento do endpoint:** se o PC de um membro é invadido, o conteúdo daquele membro vaza. E2EE protege o caminho, não a ponta.
- **Confiança no convite:** quem tem o código entra. A responsabilidade de a quem dar o código é do dono da sala. Não há moderação central porque não há central.

### Resumo em uma linha
> Anônimo e cego ao conteúdo para todo mundo — **exceto** que quem hospeda vê IPs e metadados. Use VPN se o IP importar.

## 7. Verificação de peers (anti-MITM)

O §3 impede o relay de se admitir, mas um membro malicioso ou um link vazado ainda podem colocar alguém indesejado. Verificação opcional fora de banda:

- Cada sala tem uma **palavra-chave de segurança** (safety number) derivada do estado do grupo MLS, igual ao Signal.
- Dois membros podem comparar a palavra por fora (áudio, mensagem) pra confirmar que ninguém extra está no grupo.
- A lista de membros (MemberIds + apelidos) fica sempre visível na UI, e toda entrada gera um aviso na sala.
- Safety number é fase futura e não bloqueia o MVP. O aviso de entrada faz parte do MVP.

## 8. Armazenamento local e mensagens offline

- Chave privada: cifrada em repouso (DPAPI ou senha, §1).
- Histórico de texto: opcional, **local**, cifrado. O relay nunca guarda histórico legível.
- **Mensagens offline no MVP: não existem.** Uma mensagem só chega a quem está online no momento do envio. Uma fila cifrada no relay fica para avaliação na Fase 7.
- Sem telemetria, sem log de uso enviado a lugar nenhum.

## 9. Critérios de aceite

- [ ] Reusar um código de convite é recusado.
- [ ] Um relay que emite um `t` válido sem conhecer `s` **não** consegue ser admitido no grupo.
- [ ] Capturar o tráfego no relay e tentar decodificar mídia → impossível sem a chave do grupo.
- [ ] Um membro removido não consegue ler mensagens enviadas após a remoção (novo epoch).
- [ ] Toda entrada na sala gera aviso visível para os membros.
- [ ] O app não envia nenhum pacote a domínios de terceiros (verificável no Wireshark).
