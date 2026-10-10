# Spike D-02/D-03 — vídeo decodificado na GPU e exibido sem passar pela CPU

Perguntas (`SPEC-01` §10, `SPEC-06` Fase 0):

- **D-03:** como decodificar HEVC/AV1 na GPU em máquinas sem a extensão HEVC da Store?
- **D-02:** a UI consegue exibir o vídeo decodificado sem cópia para a CPU? Com que custo de RAM/CPU?

## Preparação

O spike usa FFmpeg LGPL (shared) e `libclang` em `.deps/` (fora do git):

```sh
mkdir -p .deps && cd .deps
curl -LO https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip
unzip ffmpeg-n9.0-*.zip && mv ffmpeg-n9.0-latest-win64-lgpl-shared-9.0 ffmpeg
# libclang.dll: do wheel "libclang" 18.1.1 win_amd64 do PyPI (é um zip), em .deps/libclang/
# vídeo de teste (codificado na própria GPU AMD):
ffmpeg/bin/ffmpeg -f lavfi -i "testsrc2=size=1920x1080:rate=60,format=nv12" -t 20 \
  -c:v hevc_amf -usage lowlatency -b:v 4M -g 120 test-1080p60-hevc.mp4
```

`.cargo/config.toml` aponta `FFMPEG_DIR` e `LIBCLANG_PATH` para `.deps/`. Para
rodar, `.deps/ffmpeg/bin` precisa estar no `PATH` (DLLs do FFmpeg).

```sh
spike-video probe                                   # o que a GPU oferece
spike-video headless .deps/test-1080p60-hevc.mp4 15 # decode + conversão, sem janela
spike-video play .deps/test-1080p60-hevc.mp4 15     # variante A: Slint wgpu
spike-video play-overlay .deps/test-1080p60-hevc.mp4 15  # variante B: Slint software + swapchain
spike-video idle software|femtovg|wgpu 8            # custo base de cada renderer do Slint
spike-video dump .deps/test-1080p60-hevc.mp4 150 .deps/q.bmp  # confere um quadro
```

## Resultados (2026-10-09 · Ryzen 5 5600 + RX 6700 XT · Windows 10)

### Capacidades (`probe`)

| | H.264 | HEVC | AV1 |
|---|---|---|---|
| Encoder de hardware (MF) | AMF | AMF | — |
| Decoder do Media Foundation | Microsoft (software/DXVA) | **não instalado** | **não instalado** |
| Decode D3D11VA direto na GPU | sim | sim (8 e 10 bits) | **sim** |

Sem a extensão da Store, o Media Foundation não decodifica HEVC nem AV1 nesta
máquina, mas a GPU decodifica os dois pelo D3D11VA. Isso confirma a premissa
da D-03.

### Pipeline (HEVC e H.264 1080p60, ritmo real de 60 fps)

Todas as variantes: **60,0 fps, 0 quadros atrasados**. Imagem conferida
contra o decode de referência do FFmpeg: PSNR Y 38,5 dB, geometria idêntica.

| Variante | RAM working set | RAM privada | CPU (1 núcleo) | CPU (máquina) |
|---|---|---|---|---|
| Decode + conversão, sem janela | 47 MB | 69 MB | 1,5% | 0,13% |
| **A.** Slint wgpu/D3D12, vídeo como `slint::Image` | 224 MB | 423 MB | 6,0% | 0,50% |
| **B.** Slint software + swapchain D3D11 em janela-filha | **58 MB** | **74 MB** | **1,7%** | **0,14%** |

Custo base do Slint, janela parada sem vídeo:

| Renderer | RAM working set | RAM privada |
|---|---|---|
| software | **22 MB** | 20 MB |
| femtovg (OpenGL) | 159 MB | 176 MB |
| wgpu (D3D12) | 136 MB | 125 MB |

**Conclusão:** o que pesa é o driver gráfico carregado pelo renderer da UI
(+110–140 MB), não o vídeo. Com UI em GPU, a meta de RAM ociosa < 80 MB
(RNF-01) é inviável. Com a variante B, ela fica com folga.

### Como a CPU foi medida

`GetProcessTimes` (e o Gerenciador de Tarefas) **subestimam** esse tipo de
carga: o Windows cobra CPU por amostragem no tick de ~15,6 ms, e uma thread
que acorda, trabalha < 1 ms e dorme quase nunca é cobrada. A primeira medição
deu "0,00%", o que é falso. O spike conta **ciclos** (`QueryProcessCycleTime`)
e converte com a frequência medida do núcleo. Ainda é uma estimativa, porque a
frequência real varia, mas a ordem de grandeza é confiável.

## Decisões

- **D-03 → FFmpeg (build mínimo LGPL) com D3D11VA.** O caminho pelo Media
  Foundation não funciona sem a extensão da Store.
- **D-02 → variante B:** UI no **renderer software do Slint** + vídeo em
  **janela-filha com swapchain D3D11** (flip model). O processador de vídeo da
  GPU converte NV12→RGB e redimensiona direto no backbuffer. O D3D11 só existe
  enquanto há vídeo na tela.

## Consequências para o produto

- **A UI não desenha por cima do vídeo** (a janela-filha cobre a área). Nomes,
  anel de "falando" e controles sobre o vídeo precisam ficar fora da área de
  vídeo ou ser desenhados no próprio D3D11.
- **Grade de vídeos:** um backbuffer só, com o processador de vídeo fazendo um
  `Blt` por stream em retângulos diferentes. Isso evita uma janela-filha por
  participante.
- **Redimensionamento da janela** precisa reposicionar a filha e chamar
  `ResizeBuffers` (não feito no spike).
- O **binário cresce** com o FFmpeg. Medir o tamanho com um build estático e
  mínimo (só parsers + hwaccel de H.264/HEVC/AV1) antes da Fase 4.

## O que este spike NÃO provou

- Outras GPUs (NVIDIA, Intel) e Windows 11. Só testado na máquina de referência.
- Vídeo real de tela/jogo (o teste usa `testsrc2`, sintético).
- Sincronização correta na variante A (usa espera por *query* + rodízio de 3
  texturas, sem fence compartilhada). Irrelevante se a variante B for a escolhida.
- Medição da CPU pelo mesmo critério do Gerenciador de Tarefas em outra máquina.
