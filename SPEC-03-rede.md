# SPEC-03 — Rede, NAT e Modos de Host

Status: **Draft** · Fase: 0 · Depende de: SPEC-01, SPEC-02

## 1. Transporte: QUIC sobre UDP

- Toda a comunicação (sinalização, texto, mídia) roda sobre **QUIC**, via **iroh** (D-01, §4.4).
- **UDP** é obrigatório para tempo real: quando um pacote se perde, não trava os outros. TCP (e qualquer túnel só-TCP) causaria head-of-line blocking — um pacote perdido segura todo o fluxo, gerando engasgo e latência. Isso mata o RNF-06.
- QUIC dá streams independentes: texto e sinalização num stream confiável, mídia em **datagramas não-confiáveis** (perda é tolerável em voz/vídeo; melhor perder um frame que atrasar).

## 2. Por que Cloudflare não resolve

Avaliado e descartado como solução de acesso:

- **Cloudflare Tunnel** é HTTP/TCP. Não expõe UDP público sem que **cada cliente** instale o `cloudflared`, o que quebra o "baixa o `.exe` e pronto".
- Forçar mídia por TCP/WebSocket pelo túnel reintroduz head-of-line blocking → engasgo e latência → fere o princípio nº 1 (desempenho).
- O produto que faz UDP de verdade (**Cloudflare Spectrum**) é enterprise e caro.
- Qualquer serviço de terceiro no caminho também contraria o "zero terceiros" (`SPEC-02` §9).

Conclusão: Cloudflare fica fora do caminho de mídia. As opções reais estão abaixo.

## 3. Os dois modos de host (decisão do usuário)

### Modo x1 (P2P direto)
- Dois participantes. Um cria a sala e vira **dono + relay** (host interno, ver `SPEC-02` §2).
- Conexão **direta** entre os dois via QUIC. Latência mínima, nada no meio.
- **Restrição importante:** um link de convite é estático, então não serve de canal de sinalização. Sem um terceiro ponto online, o x1 só funciona se o **criador for alcançável** de fora (IPv6, porta mapeada automaticamente ou porta aberta à mão). Detalhes em §4.
- Se o criador não for alcançável, o x1 precisa de um **ponto de encontro** (o relay de um modo servidor que o usuário já tenha) pra coordenar o hole punching ou repassar o tráfego. Ver §4.3.
- Se o criador fecha o app, a sala acaba.

### Modo servidor (SFU)
- Uma máquina dedicada roda o binário headless do relay (`lerrichat.exe --host` no Windows, `lerrichat-host` no Linux; ver `SPEC-01` §2).
- Opções de onde rodar:
  - **Container LXC no Proxmox de casa** (o caso do Lerri): leve, sobra recurso. Precisa de porta UDP acessível.
  - **VPS barata** (~R$15–25/mês): já vem com IP público, resolve NAT de uma vez.
- Aguenta 3–10 pessoas e sobrevive a qualquer cliente sair.
- Também serve de **STUN, ponto de encontro e relay de último caso** para salas x1 (§4.3).

A escolha é do usuário na criação da sala. O app suporta os dois; o convite carrega como achar o relay.

## 4. Conectividade e NAT traversal

### 4.1 Candidatos no convite
O campo `h` do convite carrega **uma lista de candidatos**, testados em ordem e em paralelo (estilo "happy eyeballs"):

1. **IPv6 global** do relay — a maioria das operadoras já entrega e IPv6 normalmente não tem NAT.
2. **IPv4 público mapeado** — porta aberta automaticamente no roteador via **PCP / NAT-PMP / UPnP-IGD** (§4.2), ou aberta à mão.
3. **Hostname** (modo servidor com DNS próprio).
4. **IP da LAN** — pra quando os dois estão na mesma rede.
5. **IP de VPN overlay** (opcional) — se o criador já usa Radmin VPN (`26.0.0.0/8`), Tailscale (`100.64.0.0/10`), ZeroTier ou WireGuard, o IP dessa interface entra como candidato. Quem estiver na mesma VPN conecta direto por ela, sem infra do Lerri Chat. A VPN **nunca é obrigatória**: é só mais um caminho, e quem não a tem usa os outros candidatos.

### 4.2 Modo x1 sem infra
O criador, ao abrir a sala:

1. Pede ao roteador um mapeamento de porta UDP via **PCP → NAT-PMP → UPnP-IGD** (nessa ordem). Esses protocolos falam só com o roteador local — não há terceiro envolvido. O mapeamento é renovado enquanto o app está aberto e liberado ao fechar.
2. Coleta o IPv6 global, se houver.
3. Põe os candidatos no convite.

Se nenhum candidato for alcançável (sem IPv6, roteador sem PCP/UPnP, ou **CGNAT**), a UI avisa na hora da criação: *"Seu PC não é alcançável de fora. Use um servidor como ponto de encontro ou abra uma porta."*

### 4.3 Modo x1 assistido por relay
Quando o usuário tem um relay (modo servidor) configurado, o x1 pode usá-lo **só para coordenação**:

1. **STUN embutido no relay** — cada peer descobre seu IP:porta público. **Não se usa STUN público** (contraria o "zero terceiros").
2. **Sinalização** — os dois peers trocam candidatos pelo relay, em tempo real.
3. **Hole punching** — os dois mandam pacotes UDP ao mesmo tempo pra abrir o caminho nos NATs. Se der certo, a mídia vai **direto**, sem passar pelo relay.
4. **Relay de último caso** — se o furo falhar (ex.: CGNAT dos dois lados), o próprio relay repassa os pacotes cifrados. Continua cego.

### 4.4 Implementação: iroh com relay próprio (D-01, decidido)
Hole punching sobre QUIC não vem pronto no `quinn`. Opções avaliadas no spike da Fase 0 ([`spikes/p2p`](spikes/p2p/README.md)):

| Opção | Prós | Contras |
|-------|------|---------|
| **iroh 1.x** (n0, Rust, QUIC sobre `noq`, um fork do `quinn`, com hole punching e relays) | Resolve sinalização, hole punching, mapeamento de porta (PCP/UPnP) e fallback; maduro | Por padrão usa relays e DNS da n0 → **roda só com relay próprio**; o relay repassa por HTTPS/TCP no último caso; +~4–5 MB de binário |
| **Implementação própria** sobre `quinn` (socket UDP compartilhado entre STUN, punching e QUIC) | Controle total, binário menor | Meses de trabalho e muitos casos de borda de NAT |

**Escolhido: iroh.** Configuração obrigatória:
- `presets::Minimal` + `RelayMode::Custom(<relay do usuário>)`. Nunca `presets::N0` nem `RelayMode::Default`.
- O relay do Lerri Chat embute o `iroh-relay` ao lado do SFU.
- A chave de transporte do iroh **não** é a chave de identidade do membro (`SPEC-02` §1). Usar chave de transporte própria, para o relay não ligar a mesma pessoa entre salas.

Resultado do spike, na mesma máquina: conectou só pelo relay em ~31 ms, migrou para caminho direto em ~150 ms e entregou 1000/1000 datagramas. **Falta validar com NAT real (Fase 1).**

## 5. Cenários de rede do host caseiro (Proxmox)

O gargalo do host caseiro é o **upload** e o **tipo de IP**.

### Checar CGNAT
Compare o IP WAN que aparece no painel do roteador com o que aparece em `ipinfo.io`.
- **Iguais** → você tem IP público. Basta **abrir uma porta UDP** no roteador apontando pro container. Menor latência, de graça.
- **Diferentes** → **CGNAT**. Abrir porta não funciona. Opções:
  1. Pedir **IP público** à operadora (às vezes grátis, às vezes ~R$10–20/mês).
  2. **IPv6** — o relay escuta em IPv6 e o convite o lista primeiro (§4.1).
  3. **VPS mínima com WireGuard** ligada ao Proxmox, só repassando UDP. A VPS vê apenas pacotes cifrados; o processamento continua no servidor de casa.

### Orçamento de banda (upload do host)
O SFU reenvia cada stream pra cada destinatário.

| Cenário | Conta | Upload necessário |
|---------|-------|-------------------|
| 3 pessoas, 1 compartilha tela 3 Mbps | 2 destinatários × 3 Mbps | ~6 Mbps |
| 10 pessoas, 1 compartilha tela 4 Mbps | 9 × 4 Mbps | ~36 Mbps |
| 10 pessoas só voz, todos falando | 9 × ~32 kbps × 10 origens | < 3 Mbps |

Com o VAD (só transmite quem fala), a voz real fica bem abaixo disso. Some ~5–10% de overhead de QUIC + SFrame.

Com **350 Mbps de upload** (caso do Lerri), mesmo o pior cenário (~36 Mbps) é ~10% da banda. Sobra folga enorme.

### Carga de CPU do SFU
O SFU **não decodifica vídeo** — só copia pacotes cifrados. Um **i5 de 8ª geração** aguenta muito mais que 10 pessoas. GPU no servidor é irrelevante (não é usada).

## 6. Controle de congestionamento e adaptação

### 6.1 QUIC × controle de bitrate da mídia
Datagramas QUIC **também passam pelo controle de congestionamento do QUIC**. Se o encoder mandar mais do que a janela do QUIC permite, os datagramas ficam na fila (latência) ou são descartados — e os dois controles brigam entre si. Regras:

- Usar **BBR** no QUIC do iroh (menos sensível a perda isolada e não enche buffer como o Cubic). *Confirmar na Fase 3 se o iroh 1.x expõe a escolha do controlador.*
- **Fila de envio de datagramas curta**: datagrama velho é descartado, nunca atrasado (melhor perder que enfileirar).
- **Controle de bitrate da mídia na aplicação**, com feedback do receptor (perda e atraso por número de sequência do cabeçalho SFrame) + RTT/janela do QUIC. O encoder é configurado **abaixo** da taxa que o QUIC estima disponível.
- Prioridade quando falta banda: **voz > tela > câmera**.

### 6.2 Simulcast (só modo servidor)
Para que cada destinatário receba o que a conexão dele aguenta (codec em `SPEC-04`):

- Quem transmite tela/câmera envia **2–3 qualidades** (ex.: 1080p alto, 720p médio, 360p baixo).
- O SFU encaminha, pra cada destinatário, a camada que cabe na banda dele, lendo a camada no cabeçalho SFrame em claro (`SPEC-02` §4.3).
- No **x1 não há simulcast**: só existe um receptor, então o transmissor adapta o próprio bitrate direto.

## 7. Portas e descoberta

- O relay usa **uma porta UDP** configurável (padrão sugerido: 47xxx aleatório por instalação).
- O convite carrega a lista de candidatos (§4.1). Sem diretório, sem descoberta na LAN por padrão (pode virar opção futura).
- **Firewall do Windows:** quem escuta (criador no x1, relay no Windows) dispara o prompt do Defender na primeira vez. A UI explica antes de o prompt aparecer (ver `SPEC-01` §9).

## 8. Critérios de aceite

- [ ] x1 sem infra: o criador com IPv6 **ou** porta mapeada por PCP/UPnP recebe a conexão do convidado.
- [ ] x1 sem infra e criador inalcançável: a UI avisa **na criação da sala**, não na falha do convidado.
- [ ] x1 assistido: dois peers em NATs comuns conectam por hole punching coordenado pelo relay, com mídia direta (sem passar pelo relay).
- [ ] Cliente conecta ao SFU atrás de CGNAT via IPv6.
- [ ] Com o SFU no Proxmox e porta UDP aberta, 3 clientes em redes diferentes entram na mesma sala.
- [ ] Derrubar a conexão de um cliente não afeta os outros (streams independentes).
- [ ] Sob perda/limitação de banda simulada, a voz se mantém e o vídeo cai de camada, sem latência crescente.
- [ ] Mídia nunca cai pra TCP no caminho feliz.
- [ ] Nenhum STUN ou relay de terceiros é contatado.
