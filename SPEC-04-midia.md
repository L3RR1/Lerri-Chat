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

Entrou depois da Fase 6:
- **Push-to-talk:** tecla global lida pelo `GetAsyncKeyState` na própria thread de áudio a cada 10 ms (sem hook, sem admin; vale com o jogo em foco). Qualquer tecla ou botão do mouse, gravada na hora. Com a tecla apertada transmite mesmo sem o VAD detectar fala e segura 150 ms ao soltar.
- **Escolha de microfone e saída** pelo nome, com "Padrão do Windows" como opção. Trocar na voz reabre só o áudio. Um escolhido que sumiu cai no padrão.

- **Segue o padrão do Windows:** o lado que está em "Padrão do Windows" acompanha a troca do padrão (fone ligado, saída trocada no ícone de som) sem sair da voz. A thread de voz olha o padrão a cada 1 s e reabre só os streams, mantendo o AEC e o encoder. Um dispositivo que some no meio da chamada é reaberto no padrão (até 3 s esperando o Windows eleger o novo); só sai da voz se não houver nenhum. A sala avisa para onde foi.

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
4. **H.264 por software** — só último caso, com aviso de que vai pesar. *Implementado com o encoder H.264 que vem no Windows (Media Foundation), não com o OpenH264 (ver `SPEC-01` §1): síncrono, uma cópia GPU→CPU por quadro (textura de leitura), no modo mais rápido. A tela vai em até 720p30 e sem camada baixa (dois encoders na CPU pesariam o dobro); a câmera já é 720p30. Medido no Ryzen de referência: ~10 ms por quadro 720p. `LERRICHAT_SOFTWARE_ENCODE=1` finge que a placa não tem encoder, para testar esse caminho. Falta nas edições "N" do Windows sem o Media Feature Pack.*

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
| Prévia | Quem liga a câmera se vê na grade da sala ("Sua câmera", fora do limite de 4 assistidos). Cada quadro lido vai também para uma janela-filha própria, espelhada, direto da textura da câmera: sem encode, decode nem rede, e funciona com ninguém assistindo. Abre sozinha ao ligar a câmera; o "x" ou "Esconder prévia" fecha. A janela só é destruída depois que a thread da câmera larga a swapchain (`Command::Preview`). |

A C920 de referência entrega 15 fps num quarto escuro (a exposição automática alonga o quadro) e 30 fps com luz. Desligar a prioridade de exposição daria 30 fps sempre, com a imagem mais escura; ficou como está.

## 4. Decode e render (lado de quem recebe)

- Decode por hardware (**D3D11VA**) → processador de vídeo do D3D11 (NV12→RGB + escala) → backbuffer da swapchain da janela-filha de vídeo, **sem passar pela CPU** (D-02, validado em `spikes/video`). A UI do Slint não desenha por cima dessa área.
- O decoder HEVC do Media Foundation pode depender da extensão "HEVC Video Extensions" da Store. Por isso o decode usa D3D11VA direto (build mínimo do FFmpeg), com o MFT como alternativa quando disponível.
- **Só decodifica o que está visível.** Transmissão minimizada ou aba trocada → decoder é liberado. Economia direta de CPU/GPU.
- Cada destinatário recebe só a camada de simulcast que cabe na banda dele. *Implementado depois da Fase 6, para a tela:*
  - duas camadas, a alta (até 1080p, bitrate adaptativo) e a baixa (360p, 30 fps, 400 kbps);
  - a baixa só é codificada enquanto o relay diz que alguém precisa dela (`Watchers.low`);
  - o relay decide por pessoa: quem atrasa na alta cai para a baixa a partir do keyframe dela, e volta depois de 10 s sem atrasar;
  - cada camada tem a sua sequência, e o receptor troca de camada num keyframe, com decoder novo;
  - a câmera não tem simulcast (já é leve, e cai antes da tela).
- **Várias telas ao mesmo tempo:** até 4 vídeos (telas e câmeras juntas), cada um com decoder, thread e janela-filha próprios. O Rust divide a área numa grade (as colunas que deixam cada vídeo 16:9 maior; última linha centralizada) e passa a posição de cada célula ao Slint, que só desenha cabeçalho e caixa preta. Mover ou fechar uma janela-filha força um redesenho da janela inteira: o renderer só repinta o que mudou, e o que ele pinta embaixo de uma janela-filha não chega à tela.
- Compartilhar **uma janela só**: o mesmo caminho do WGC (`CreateForWindow`). Fechar a janela encerra o compartilhamento com aviso; redimensionar encaixa a imagem no tamanho inicial, com barras.

## 5. Simulcast (resumo; rede em SPEC-03)

- **Só no modo servidor.** No x1 há um único receptor e o transmissor adapta o bitrate direto.
- Transmissor envia 2–3 qualidades da mesma fonte. Cada camada é uma **sessão de encoder** separada.
- GPUs NVIDIA de consumo limitam o número de sessões NVENC simultâneas. Tela + câmera com 3 camadas cada são 6 sessões. O app conta as sessões disponíveis e reduz camadas se precisar.
- Onde houver **AV1 com SVC** (escalabilidade temporal/espacial num único stream), preferir SVC a simulcast: uma sessão de encoder só. *Implementado depois da Fase 6, temporal (L1T2):*
  - o encoder AV1 recebe `CODECAPI_AVEncVideoMaxTemporalLayers`/`TemporalLayerCount` = 2; se aceitar, metade dos quadros (camada temporal 1) não é referência para ninguém;
  - a camada é lida do próprio bitstream (extensão do cabeçalho da OBU de quadro; no HEVC, `nuh_temporal_id_plus1`) e vai no cabeçalho em claro como "descartável" (bit 2 do byte 1, compatível com quem não conhece o bit);
  - o descartável leva a sequência do quadro de base de que depende, sem avançá-la: o relay corta e o receptor não vê buraco;
  - o relay desce uma escada para quem atrasa: com metade do limite em trânsito, para de mandar os descartáveis (metade do fps, sem keyframe); atrasando de verdade, cai para a camada baixa do simulcast (que só é codificada se alguém precisar); 10 s calmo, sobe de volta;
  - ligado por padrão só no AV1. O mecanismo foi testado ponta a ponta com o HEVC da RX 6700 XT (AMF aceita L1T2: só a base decodifica sem erro); o AV1 por hardware não dá para testar aqui (a placa não codifica AV1). O H.264 da AMD aceita a configuração mas não marca a camada no bitstream, então nada sai descartável.
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
