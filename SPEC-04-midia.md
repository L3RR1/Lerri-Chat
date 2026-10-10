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

### 1.1 Como ficou na Fase 3 (`crates/media`)

| Peça | Implementação |
|------|---------------|
| Captura/saída | `cpal` sobre WASAPI compartilhado, dispositivos padrão. Uma thread "voz" converte para 48 kHz mono e trata o microfone; o callback de saída só mistura. Só Windows; o resto do pipeline é portável e roda nos testes de qualquer plataforma. |
| AEC3 + NS + AGC2 + passa-alta | `aec3`, porte em Rust do WebRTC APM, quadros de 10 ms. A referência do AEC é a mistura que acabou de ir para a saída. |
| Supressão forte | `nnnoiseless` (RNNoise), opção na tela da sala; a probabilidade de voz dele também entra no VAD. |
| VAD | Energia acima do piso de ruído adaptativo (+9 dB), com 300 ms de *hangover* para não cortar fim de palavra. |
| Opus | `unsafe-libopus`, VOIP, 20 ms, 32 kbps, complexidade 5, FEC in-band. Sem silêncio no fio, ~4 kB/s por quem fala. |
| Jitter buffer | Por remetente, alvo de 20 a 160 ms guiado pelo jitter medido (RFC 3550). Perda: FEC do pacote seguinte, senão PLC. Atraso acima do alvo descarta o mais velho. |
| Presença | "Falando agora" = pacote recebido nos últimos 300 ms (ou o próprio VAD, para você). Atualiza a cada 250 ms. |

Medido (`cargo test --release -p lerri-media -- --ignored cpu_cost`, Ryzen 5 5600): **1,5% de um núcleo, 0,12% da máquina** falando sem parar, contando tratamento, Opus e SFrame de quem fala e decifrar, buffer, decode e mistura de quem ouve. O AEC derruba um eco sintético em mais de 15 dB nos testes.

Orçamento de latência boca-a-ouvido (estimado, ainda não medido com áudio real entre dois PCs): buffer de captura do WASAPI (~10 ms) + quadro Opus (20 ms) + rede (RTT/2) + relay + buffer de jitter (20–40 ms em rede boa) + buffer de saída (~10–20 ms) ≈ **60–90 ms + rede**.

Fica para depois: **push-to-talk** (precisa de atalho global no Windows), escolha de dispositivo (hoje usa o padrão) e trocar o dispositivo padrão sem sair e entrar de novo na voz.

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

### 2.1 Como ficou na Fase 4 (`crates/media/src/video`, `crates/media/src/screen.rs`)

| Peça | Implementação |
|------|---------------|
| Captura | WGC do monitor inteiro (o principal, ou o escolhido na lista). O callback só copia o quadro novo para uma textura "mais recente" na GPU; o encoder pega no próprio ritmo. Uma rajada de mudanças vira um quadro, e o último estado de uma tela que parou nunca se perde. Sem fallback DXGI ainda. |
| Escala e cor | Processador de vídeo do D3D11: BGRA → NV12 (BT.709 faixa limitada) e redução para caber em 1920×1080 (ou 1080×1920 com monitor em pé), com lados pares. |
| Encode | MFT de hardware do Media Foundation, modo assíncrono, entrada NV12 direto da textura (DXGI device manager). Baixa latência, sem B-frames, `LowDelayVBR` (ou CBR), GOP longo: keyframe só sob pedido (`CODECAPI_AVEncVideoForceKeyFrame`). Teto de 60 fps; tela parada não gera quadro. |
| Bitrate | Começa em 3 Mbps, entre 0,3 e 5 Mbps. Desce 25% quando há 3+ quadros em trânsito sem confirmação do relay, sobe 15% depois de 3 s fluindo, e com 6+ em trânsito nem codifica o próximo quadro. |
| Negociação | Cada membro anuncia pelo MLS os codecs que decodifica (perfis D3D11VA + FFmpeg presente). O transmissor usa o melhor que ele codifica e todos os outros decodificam; sem nada em comum, H.264. Renegocia a cada 250 ms e recria o encoder com keyframe. |
| Decode | FFmpeg com D3D11VA, carregado em tempo de execução: as DLLs (`avcodec-63`, `avutil-61`, `swresample-7`) vão ao lado do `.exe` ou em `ffmpeg\`. Sem elas o app abre normalmente e só não assiste. Os tipos do FFmpeg ficam pré-gerados em `video/ffi.rs`, sem FFmpeg nem libclang no build. |
| Exibição | Janela-filha com swapchain D3D11 (flip model) sobre a área de vídeo da sala, com barras pretas se a proporção não bater. A UI cria, move e destrói a janela; a thread de vídeo só desenha. Redimensionar pede keyframe, porque a imagem some. |
| Só o visível | "Fechar" encerra decoder e janela. Com a janela principal minimizada, o relay para de mandar e volta com keyframe. Sem ninguém assistindo, quem compartilha nem codifica. |

Medido na RX 6700 XT, com a tela em movimento o tempo todo: transmissor a **0,09% da máquina** (1,1% de um núcleo) e 67 MB; receptor decodificando 58 quadros/s com 53 MB, sem janela. Ida e volta tela → encoder → decoder: PSNR 38–58 dB conforme o conteúdo, HEVC e H.264. A CPU de quem recebe, pela contagem do Windows, fica perto de zero, o que subestima trabalho curto e frequente (ver `spikes/video`).

## 3. Câmera

- Captura via **Media Foundation** (`IMFSourceReader`).
- Reaproveita **o mesmo pipeline de encode/decode** da tela (AV1/HEVC/H.264 por HW).
- Resoluções típicas: 720p30 a ~1–1.5 Mbps, adaptativo.
- Câmera e tela são dois streams independentes; dá pra mandar os dois ao mesmo tempo.

### 3.1 Como ficou na Fase 5 (`crates/media/src/video/camera.rs`)

| Peça | Implementação |
|------|---------------|
| Captura | `IMFSourceReader` com o nosso dispositivo D3D11 (`MF_SOURCE_READER_D3D_MANAGER`) e processamento avançado: o leitor converte MJPEG/YUY2 para NV12 e entrega o quadro já como textura na GPU. |
| Modo da câmera | O nativo mais perto de 720p30, preferindo MJPEG: vídeo cru em 720p30 (~40 MB/s) passa da banda real do USB 2.0, e a câmera cai de taxa sozinha (a C920 caía para 13 fps em YUY2). |
| Encode | O mesmo encoder de hardware da tela, 720p (cabe em 1280×720), até 30 fps, bitrate entre 0,2 e 1,5 Mbps (começa em 1,2). |
| Transporte | Mesmo caminho da tela (`SPEC-03` §6.3), com tipo "câmera" no cabeçalho. O relay desiste antes da câmera do que da tela quando alguém atrasa. |
| Sem plateia | O encoder para, mas a câmera continua sendo lida (e a luz dela fica acesa) para não acumular quadros velhos. |

A C920 de referência entrega 15 fps num quarto escuro (a exposição automática alonga o quadro) e 30 fps com luz. Desligar a prioridade de exposição daria 30 fps sempre, com a imagem mais escura; ficou como está.

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
