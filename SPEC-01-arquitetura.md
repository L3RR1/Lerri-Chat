# SPEC-01 — Arquitetura

Status: **Draft** · Fase: 0 · Depende de: SPEC-00

## 1. Decisão de stack

| Camada | Escolha | Por quê |
|--------|---------|---------|
| Linguagem | **Rust** | Nativo, sem garbage collector, controle de memória, binário pequeno e sem runtime. |
| UI | **Slint com renderer software** (D-02) | Nativa, sem Chromium e **sem driver gráfico carregado**: 22 MB ociosa. Os renderers de GPU do Slint custam +110–140 MB só de driver (`spikes/video`). Licença royalty-free do Slint exige atribuição no app. |
| Exibição de vídeo | **Janela-filha com swapchain D3D11** (flip model), alimentada pelo processador de vídeo da GPU (D-02) | Conversão NV12→RGB e redimensionamento na GPU, direto no backbuffer. Sem cópia para a CPU, e o D3D11 só existe enquanto há vídeo na tela. |
| Transporte + NAT traversal | **QUIC via iroh** (fork `noq` do quinn) com **relay próprio** (D-01) | UDP, baixa latência, streams independentes e datagramas, com hole punching, mapeamento de porta (PCP/UPnP) e relay de último caso prontos. Configurado sem nenhum serviço da n0 (`SPEC-03` §4.4). |
| Criptografia de grupo | **MLS** via `openmls` | Padrão IETF (RFC 9420) pra E2EE em grupo, com novo epoch a cada entrada/saída. |
| Criptografia de mídia | **SFrame** (RFC 9605) com chaves do MLS exporter | Chave por remetente; cabeçalho mínimo em claro para o SFU rotear sem decifrar (`SPEC-02` §4.3). |
| Captura de tela | **Windows.Graphics.Capture** (WGC) + fallback **DXGI Desktop Duplication** | Captura direto da GPU, sem copiar frame pra CPU. |
| Captura de câmera | **Media Foundation** (`IMFSourceReader`) | API nativa de câmera do Windows. |
| Encode de vídeo | **AMF** (AMD), **NVENC** (NVIDIA), **Quick Sync** (Intel) via MFT de hardware do Media Foundation | Codifica na GPU. Codec por hardware: AV1 onde houver, senão HEVC (H.265), senão H.264. |
| Encode por software (último caso) | **OpenH264** (BSD) | Evita a GPL do x264. |
| Decode de vídeo | **D3D11VA via build mínimo do FFmpeg** (LGPL, só parsers + hwaccel de H.264/HEVC/AV1) (D-03) | Decodifica na GPU. Na máquina de referência, o Media Foundation não tem decoder HEVC nem AV1 sem a extensão da Store, mas a GPU decodifica os dois pelo D3D11VA (`spikes/video`). |
| Áudio — codec | **Opus** (`opus` crate) | Padrão de voz em tempo real, leve e excelente a baixo bitrate. |
| Áudio — processamento | **WebRTC Audio Processing** (AEC3 + supressão de ruído + AGC); **RNNoise** como supressão forte opcional | Cancelamento de eco é difícil de fazer do zero; o módulo do WebRTC é o padrão de mercado. |
| Captura/saída de áudio | **WASAPI** (modo compartilhado, baixa latência) | API nativa de áudio do Windows. |
| Chave local em repouso | **DPAPI** (ou Argon2id com senha) | Proteção nativa do Windows, sem senha obrigatória. |

### Por que não Electron/web
Um app web embutido carrega um Chromium inteiro (centenas de MB de RAM, partida lenta). Isso mata o RNF-01 e RNF-09 antes de começar. A UI nativa é inegociável para as metas de desempenho.

## 2. Um binário, dois papéis

No Windows, o mesmo `.exe` tem dois modos de execução, escolhidos por flag ou pela UI:

```
lerrichat.exe                 # abre a UI (cliente)
lerrichat.exe --host          # roda só o relay, sem UI (modo servidor/serviço)
```

- **Cliente:** UI + pilha de mídia + conexão QUIC. É o que o convidado usa.
- **Relay (host):** processo headless que aceita conexões, valida o token de roteamento dos convites, ordena commits MLS e repassa pacotes cifrados (papel "relay" em `SPEC-02` §2). Vira serviço do Windows.
- **Linux (Proxmox/VPS):** build separado e headless, **`lerrichat-host`**, contendo só `core`, `crypto` (parte pública), `net` e `host` — sem `ui` nem `media`. Roda como unidade systemd.

No **modo x1**, o cliente embute o relay no mesmo processo: o participante que cria a sala sobe o relay interno e o outro conecta.

## 3. Módulos (crates do workspace)

```
lerri-chat/
├── crates/
│   ├── core        # tipos compartilhados: IDs, sala, membro, papéis, mensagens de protocolo
│   ├── crypto      # identidade (par de chaves), MLS, SFrame, convites (t/s), DPAPI
│   ├── net         # QUIC + NAT traversal via iroh (D-01), relay próprio, sinalização
│   ├── media       # captura, encode/decode, Opus, processamento de áudio, simulcast, rate control
│   ├── host        # relay/SFU: roteamento de pacotes cifrados, Delivery Service MLS, token de convite
│   ├── client      # orquestra net+media+crypto para a UI
│   └── ui          # Slint (renderer software): telas, estado; janela-filha de vídeo
├── app/            # lerrichat.exe — junta client+ui+host
└── host-bin/       # lerrichat-host — relay headless para Linux
```

Regra: `ui` só conversa com `client`. `host` não depende de `ui` nem de `media` (ele não toca em mídia em claro — só roteia bytes cifrados).

## 4. Topologia de mídia

### Modo x1 (P2P)
```
[Peer A: dono + relay interno] <===== QUIC direto =====> [Peer B]
```
Mídia cifrada ponta a ponta. O relay interno do A só existe pra aceitar a conexão; o conteúdo é cifrado com chaves que os dois derivam do grupo MLS. Conectividade (IPv6, porta mapeada, x1 assistido) em `SPEC-03` §4.

### Modo servidor (SFU)
```
            ┌──────── relay cego (SFU) ────────┐
[Peer A] ===│  roteia pacotes cifrados por sala │=== [Peer C]
[Peer B] ===│  nunca decodifica nada            │=== [Peer D]
            └──────────────────────────────────┘
```
**SFU (Selective Forwarding Unit):** o relay recebe o stream de cada transmissor **uma vez** e reencaminha pra cada destinatário. Não decodifica nem recodifica — só copia pacotes, escolhendo a camada de simulcast pelo cabeçalho SFrame em claro. Por isso aguenta várias pessoas num i5 sem GPU.

### Por que não malha P2P pura com 3+ pessoas
Em malha, quem compartilha tela precisa **enviar o stream N-1 vezes** (uma cópia por pessoa). Com tela de 4 Mbps e 5 pessoas, são ~16 Mbps de upload no transmissor. Com SFU, o transmissor envia **uma vez** (4 Mbps) e o relay se vira. Daí o modo servidor existir.

## 5. Pipeline de mídia (visão)

Transmissão de tela, fim a fim:

```
WGC/DXGI (GPU) → encoder HW (GPU, AV1/HEVC/H.264) → cifra SFrame (chave do remetente) →
  datagrama QUIC → [relay copia] → decifra SFrame →
  decode HW (D3D11VA, GPU) → processador de vídeo → swapchain da janela-filha
```

Nenhum frame cru passa pela CPU no caminho feliz. O "render" é o processador de vídeo do D3D11 escrevendo no backbuffer da swapchain da janela-filha (D-02). Detalhes e parâmetros em `SPEC-04`.

## 6. Modelo de dados essencial (`core`)

```rust
RoomId      = [u8; 16]        // aleatório
MemberId    = PublicKeyHash   // derivado da chave pública do membro
Role        = Owner | Member  // o relay não é membro (SPEC-02 §2)
Invite      = { room_id, candidates: Vec<HostAddr>, route_token, invite_secret,
                inviter_fpr, expires_at }   // relay só recebe hash(route_token)
RoomConfig  = { mode: X1 | Server, max_members, media_caps }
```

Mensagens de protocolo (texto, presença, controle de mídia) são enumeradas em `SPEC-02`/`SPEC-03` e sempre viajam cifradas.

## 7. Threads e concorrência

- Runtime **async (tokio)** para rede.
- Captura/encode de mídia em **thread dedicada** por stream (não bloqueia a rede).
- UI em thread própria, comunicando com `client` por canais.
- Decode: só é instanciado para streams **visíveis na tela**. Minimizar uma transmissão libera o decoder — economia direta de CPU/GPU.

## 8. Portabilidade

Alvo primário: **Windows 10/11 x64**. O `core`, `crypto`, `net` e `host` são portáveis (o relay roda em Linux no Proxmox como `lerrichat-host`). A parte dependente de plataforma (`media`, captura de tela/câmera/áudio) fica isolada atrás de traits, com implementação Windows primeiro. Linux/mac como cliente ficam como fase futura.

## 9. Distribuição no Windows (fricção real)

A meta "convidado leigo entra em < 60 s" (`SPEC-00`) esbarra em barreiras do próprio Windows:

| Barreira | Tratamento |
|----------|------------|
| **SmartScreen** ("O Windows protegeu o computador") em `.exe` sem assinatura | Orçar **assinatura de código** (certificado OV ou serviço de assinatura em nuvem da Microsoft; checar elegibilidade para pessoa física no Brasil). Até lá, a página de download explica o "Mais informações → Executar assim mesmo". |
| **Antivírus** desconfiando de app que captura tela e abre porta UDP | Assinatura de código + envio do binário pra análise da Microsoft/fornecedores antes de cada release. |
| **Firewall do Defender** ao escutar porta (criador no x1, relay no Windows) | A UI explica antes de o prompt aparecer. O cliente que só conecta não dispara prompt. |
| **Handler `lerrichat://`** | Registrado em `HKCU\Software\Classes` na primeira execução — sem admin, sem instalador. Colar o link no app sempre funciona como alternativa. |
| **Licenças de terceiros** (Slint royalty-free, FFmpeg LGPL, OpenH264 BSD) | Tela "Sobre" com atribuições; FFmpeg em build mínimo, linkado de forma compatível com a LGPL. |

## 10. Riscos de arquitetura

| Risco | Mitigação |
|-------|-----------|
| ~~Render de vídeo sem cópia na UI~~ — **resolvido no spike** (D-02). | Vídeo em janela-filha com swapchain D3D11. Consequência: a UI não desenha por cima da área de vídeo; sobreposições (nome, anel de "falando") ficam fora dela ou são desenhadas no D3D11. |
| Diversidade de GPUs quebra o encode | Detecção em runtime + cascata AV1→HEVC→H.264→OpenH264 por software. |
| Decoder HEVC indisponível no receptor | Codec negociado pelo que **todos os receptores decodificam** (`SPEC-04` §2). |
| NAT/CGNAT impede conexão direta | IPv6 + PCP/UPnP + x1 assistido por relay + modo servidor (`SPEC-03` §4). |
| Dependência do iroh (projeto de terceiros, fork próprio do quinn) | Versão fixada; configuração sem serviços da n0 coberta por teste; o crate `net` isola a API do iroh do resto do app. |
| MLS tem curva de aprendizado | Fase 2 entrega MLS completo; um atalho simétrico só é aceitável como marco interno (`SPEC-06` Fase 2). |
| `.exe` > 25 MB com FFmpeg + WebRTC APM + Slint | Builds mínimos, LTO, `opt-level = "z"` nas crates frias; medir na CI desde a Fase 0. |
