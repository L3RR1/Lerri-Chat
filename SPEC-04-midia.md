# SPEC-04 — Mídia: Voz, Tela e Câmera

Status: **Draft** · Fase: 0 · Depende de: SPEC-01, SPEC-03

Este é o coração do "máximo desempenho". Regra de ouro: **o que a GPU pode fazer, a CPU não faz.**

## 1. Voz

| Item | Escolha |
|------|---------|
| Codec | **Opus** |
| Bitrate | 24–32 kbps (voz), adaptativo |
| Frame | 20 ms, com PLC (packet loss concealment) e FEC in-band do Opus em rede com perda |
| Captura/saída | **WASAPI** modo compartilhado, baixa latência |
| Cancelamento de eco (AEC) | **WebRTC Audio Processing — AEC3** (usa o áudio de saída como referência) |
| Supressão de ruído | **WebRTC NS** por padrão; **RNNoise** como opção "supressão forte" (teclado, ventilador) |
| Ganho | **WebRTC AGC** |
| Detecção de voz | **VAD** — só transmite quando há fala |
| Modos de envio | Ativação por voz (padrão) ou push-to-talk |

- VAD economiza banda e CPU: silêncio não gera pacote.
- Meta: call de voz consome **< 2% de CPU** (RNF-03) e banda irrisória.

## 2. Compartilhamento de tela — o pilar do desempenho

### Captura (sem tocar a CPU)
- **Windows.Graphics.Capture (WGC)** — API moderna, captura monitor ou janela direto na GPU. Frame fica numa textura D3D11; nunca é copiado pra RAM da CPU.
- O WGC **só entrega frame novo quando algo muda na tela**. Tela parada = quase nenhum frame = quase nenhum trabalho. Essa é a principal fonte de economia em tela estática.
- Fallback: **DXGI Desktop Duplication** (Windows mais antigos). Ela informa *dirty rects*, usados pra decidir se vale a pena codificar o frame.

### Encode (na GPU)
Cascata de codec por hardware, detectada em runtime:

1. **AV1** por hardware — melhor compressão. Disponível em NVIDIA RTX 40+, AMD RX 7000+, Intel Arc. *(A RX 6700 XT do Lerri NÃO codifica AV1 — cai pro próximo. Ela **decodifica** AV1.)*
2. **HEVC (H.265)** por hardware — **AMF** na AMD (RX 6700 XT usa este), **NVENC** na NVIDIA, **Quick Sync** na Intel.
3. **H.264** por hardware — compatibilidade máxima.
4. **OpenH264 por software** — só último caso, com aviso de que vai pesar.

### Negociação de codec
- O que importa é o que o **receptor decodifica**, não só o que o transmissor codifica.
- Cada cliente anuncia ao entrar: codecs que **codifica** em HW e codecs que **decodifica** em HW.
- O transmissor usa o melhor codec que ele codifica **e que todos os receptores atuais decodificam**.
- Quem entra sem suporte ao codec atual dispara renegociação: o transmissor desce de codec (no limite, H.264, que todo mundo decodifica) e envia um keyframe.

### Encoding adaptativo (o truque da eficiência)
- **Tela estática** (código, docs, navegador parado): como o WGC quase não entrega frames, o framerate efetivo cai pra 1–5 fps. Os frames que saem são P-frames quase todos em blocos *skip* (iguais à referência), que custam poucos bytes. Resultado: **0,3–1 Mbps a 1080p** (RNF-04) e **< 3% de CPU** no transmissor (RNF-08).
- Os encoders de hardware via Media Foundation **não recebem dirty rects como entrada**. A economia vem de não codificar frames repetidos e da compressão natural das regiões paradas, não de "codificar só o retângulo que mudou".
- **Tela com movimento** (jogo, vídeo): sobe pra 1080p60, bitrate **2–5 Mbps** adaptativo (RNF-05). Abaixo disso a qualidade cai por física da compressão, não por código.
- O controle de bitrate reage à banda do destinatário (`SPEC-03` §6.1) e ao conteúdo (quanto mudou).
- **Keyframe sob pedido:** o receptor pede um keyframe quando entra ou quando perde pacotes demais, em vez de o transmissor mandar keyframes periódicos caros.

### Por que isso é mais leve que o Discord
- Captura e encode na GPU → CPU quase ociosa.
- Tela parada quase não gera frame → quase não gasta rede.
- App nativo sem Chromium → a RAM e a CPU da interface somem da conta.
- Codec escolhido pelo melhor denominador comum da sala (HEVC/AV1 quando todos suportam).

## 3. Câmera

- Captura via **Media Foundation** (`IMFSourceReader`).
- Reaproveita **o mesmo pipeline de encode/decode** da tela (AV1/HEVC/H.264 por HW).
- Resoluções típicas: 720p30 a ~1–1.5 Mbps, adaptativo.
- Câmera e tela são dois streams independentes; dá pra mandar os dois ao mesmo tempo.

## 4. Decode e render (lado de quem recebe)

- Decode por hardware (**D3D11VA**) → processador de vídeo do D3D11 (NV12→RGB + escala) → backbuffer da swapchain da janela-filha de vídeo, **sem passar pela CPU** (D-02, validado em `spikes/video`). A UI do Slint não desenha por cima dessa área.
- O decoder HEVC do Media Foundation pode depender da extensão "HEVC Video Extensions" da Store. Por isso o decode usa D3D11VA direto (build mínimo do FFmpeg), com o MFT como alternativa quando disponível.
- **Só decodifica o que está visível.** Transmissão minimizada ou aba trocada → decoder é liberado. Economia direta de CPU/GPU.
- Cada destinatário recebe só a camada de simulcast que cabe na banda dele.

## 5. Simulcast (resumo; rede em SPEC-03)

- **Só no modo servidor.** No x1 há um único receptor e o transmissor adapta o bitrate direto.
- Transmissor envia 2–3 qualidades da mesma fonte. Cada camada é uma **sessão de encoder** separada.
- GPUs NVIDIA de consumo limitam o número de sessões NVENC simultâneas. Tela + câmera com 3 camadas cada são 6 sessões. O app conta as sessões disponíveis e reduz camadas se precisar.
- Onde houver **AV1 com SVC** (escalabilidade temporal/espacial num único stream), preferir SVC a simulcast: uma sessão de encoder só.
- SFU encaminha a camada adequada por destinatário, lendo o cabeçalho SFrame em claro.
- Degradação graciosa: quem tem internet ruim recebe 360p em vez de travar.

## 6. Sincronização e latência

- Mídia em **datagramas QUIC não-confiáveis**: perder um frame é melhor que atrasar.
- Jitter buffer adaptativo pequeno na recepção (meta: mínimo que evite engasgo).
- Lip-sync voz↔vídeo por timestamps comuns.
- Meta de latência de voz: **< 150 ms** boca-a-ouvido na mesma região (RNF-06).

## 7. Metas mensuráveis (resumo)

| Métrica | Meta |
|---------|------|
| CPU em voz | < 2% |
| CPU no transmissor de tela (captura + encode HW) | < 3% |
| Banda tela estática 1080p | 0,3–1 Mbps |
| Banda tela com movimento 1080p60 | 2–5 Mbps |
| Latência de voz (mesma região) | < 150 ms |
| Latência adicional do SFU | < 30 ms sobre RTT direto |

## 8. Hardware de referência (Lerri)

- **Cliente:** Ryzen 5 5600 + **RX 6700 XT** → encode **HEVC via AMF** (sem AV1 em HW), decode HEVC e AV1 em HW. Folga enorme pra todas as metas.
- **Servidor:** i5 8ª gen + 20 GB → SFU puro, sem usar GPU. Aguenta >10 pessoas tranquilo.

## 9. Riscos

| Risco | Mitigação |
|-------|-----------|
| Driver de encode HW com bug/variação entre GPUs | Cascata de codec + teste de sanidade no start + fallback software com aviso. |
| Receptor sem decoder HW pro codec do transmissor | Negociação pelo denominador comum de decode (§2); H.264 como piso universal. |
| Render sem cópia inviável na UI escolhida | Spike na Fase 0; plano B `egui` + `wgpu` (`SPEC-01` §10). |
| Limite de sessões NVENC estoura com simulcast | Contar sessões disponíveis; reduzir camadas; preferir AV1 SVC. |
| Eco em setups sem fone | AEC3 do WebRTC + recomendar fone; push-to-talk opcional. |
| Jitter em redes ruins | Buffer adaptativo + FEC do Opus + simulcast pra camada menor. |
