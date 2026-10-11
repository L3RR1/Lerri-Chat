# SPEC-06 — Roadmap por Fases (SDD)

Status: **Draft** · Depende de: todas as specs anteriores

Desenvolvimento guiado por especificação: cada fase tem escopo fechado, critérios de aceite verificáveis e só começa quando a anterior passa. Nada de pular pra mídia antes da fundação de rede funcionar.

## Decisões e pendências abertas

| ID | Decisão | Status |
|----|---------|--------|
| D-01 | NAT traversal: **iroh** com relay próprio × implementação própria sobre `quinn` (`SPEC-03` §4.4) | **Decidido: iroh**, só com relay próprio ([`spikes/p2p`](spikes/p2p/README.md)). Validar NAT real na Fase 1. |
| D-02 | UI e render de vídeo sem cópia (`SPEC-01` §10) | **Decidido: Slint com renderer software + vídeo em janela-filha com swapchain D3D11** ([`spikes/video`](spikes/video/README.md)). |
| D-03 | Decode: FFmpeg mínimo (D3D11VA) × MFT de hardware (`SPEC-01` §1) | **Decidido: FFmpeg LGPL com D3D11VA.** O MF não decodifica HEVC/AV1 sem a extensão da Store. |
| D-04 | Transporte da tela: datagramas (como a voz) × um stream QUIC por quadro | **Decidido na Fase 4: stream por quadro** (`SPEC-03` §6.3). Keyframes têm centenas de pacotes; em datagramas, 1% de perda já os estragaria. O relay compensa o atraso pulando quadros até o próximo keyframe. |
| D-05 | Assinatura de código do `.exe` (`SPEC-01` §9) | **Decidido na Fase 6: sem assinatura.** O app é para o grupo de amigos; o custo (certificado ou serviço em nuvem) não compensa agora. Na primeira vez, o SmartScreen pede "Mais informações → Executar assim mesmo", e o README explica. Rever se o app for distribuído para fora do grupo. |
| P-01 | Casar os hex de roxo com o `Color.kt` do LerriNFC (`SPEC-05`) | **Resolvida na Fase 6:** todos os tokens vêm do `Color.kt`; contraste AA conferido por teste (`crates/ui/tests/contrast.rs`). |

---

## Fase 0 — Fundação + spikes de risco
**Objetivo:** esqueleto do projeto, conexão QUIC viva e os três riscos técnicos que podem mudar a stack respondidos **antes** de escrever código de produto.

Escopo:
- Workspace Rust com os crates de `SPEC-01` (`core`, `crypto`, `net`, `media`, `host`, `client`, `ui`, `app`, `host-bin`).
- Geração de identidade local (par de chaves Ed25519) cifrada com DPAPI — `RF-01`.
- Conexão QUIC cliente↔relay com TLS 1.3 (`quinn`), echo de mensagem de teste. *(Com a D-01, o `net` migra para o iroh na Fase 1.)*
- CI de build do `lerrichat.exe` (Windows) e do `lerrichat-host` (Linux), medindo o tamanho do binário.
- **Spike D-02/D-03:** decodificar um stream HEVC 1080p60 com D3D11VA e exibir na UI sem cópia pra CPU.
- **Spike D-01:** dois peers atrás de NAT conectando via iroh com relay próprio (sem relays da n0).

Fora de escopo: UI real, convites, chat.

Aceite:
- [x] `lerrichat.exe` e `lerrichat.exe --host` sobem e trocam uma mensagem por QUIC (IPv4 e IPv6; certificado do relay fixado por fingerprint). *`lerrichat-host` no Linux: compila na CI, ainda não rodado num servidor real.*
- [x] Identidade persiste cifrada (DPAPI) entre reinícios.
- [x] Binário < 25 MB (RNF-02), partida < 1,5 s (RNF-09). *Medido sem UI: 2,7 MB e ~30 ms. Remedido na Fase 6, com UI, voz, tela e câmera: 23,1 MiB (com o ícone) e janela criada em 42–91 ms.*
- [x] Spike de vídeo: 1080p60 decodificado e exibido com CPU < 5% → D-02 e D-03 registradas. *HEVC 1080p60: 60 fps, 0,14% da máquina, 58 MB de RAM.*
- [x] Spike de rede: resultado registrado em D-01. *Conecta só pelo relay próprio e migra para direto em ~150 ms; NAT real fica para a Fase 1.*

---

## Fase 1 — Salas, convites e conectividade
**Objetivo:** criar sala, gerar e consumir convites únicos, e conectar nos dois modos.

Escopo:
- `RoomId`, `RoomConfig`, papéis Dono/Membro/Relay — `RF-02`, `SPEC-02` §2.
- Convites de uso único com `t` (roteamento) e `s` (admissão) + QR — `RF-03`, `SPEC-02` §3. Nesta fase o relay valida e consome `t`; a verificação de `s` entra na Fase 2, junto com o MLS.
- Entrada colando o link ou abrindo o `lerrichat://` (handler em HKCU) — `RF-04`.
- Conectividade x1 sem infra: IPv6 + PCP/NAT-PMP/UPnP + aviso de criador inalcançável (`SPEC-03` §4.2).
- Conectividade x1 assistida: descoberta de endereço (QAD) no relay, sinalização, hole punching e relay de último caso (`SPEC-03` §4.3).

Estado: **implementado; falta o teste com redes reais.** Interface mínima adiantada da Fase 6 (início e sala, crate `ui`); a linha de comando continua (`--x1`, `--create`, `--cli --join`, `--open`, `--register`). Detalhes em `SPEC-02` §3.4 e `SPEC-03` §4.5.

Aceite:
- [x] Criar sala nos dois modos. *Testes automatizados e binário real.*
- [x] Reuso de convite é recusado. *Também convite expirado, desconhecido e emitido por quem não é dono.*
- [ ] x1 sem infra conecta quando o criador tem IPv6 ou porta mapeada; avisa na criação quando não tem. *Implementado. Conexão testada só na mesma máquina/LAN. Falta: convidado em outra rede chegando pelo IPv6. No roteador do Lerri o UPnP falhou.*
- [ ] x1 assistido: dois peers em NATs comuns conectam por hole punching, com mídia direta. *Implementado. Na mesma máquina, conecta só pelo ponto de encontro e migra para direto. Falta: dois PCs em redes diferentes.*
- [ ] Cliente alcança o SFU no Proxmox com porta UDP aberta ou via IPv6. *Falta rodar o `lerrichat-host` no servidor.*
- [ ] Nenhum STUN/relay de terceiros é contatado. *Garantido pela configuração (preset `Minimal`, só o relay do usuário, resolvedor só com o DNS do sistema, checagem de portal cativo desligada). Falta confirmar com captura de rede.*

---

## Fase 2 — Chat de texto E2EE e gestão de membros
**Objetivo:** mensagens em grupo, cifradas, em tempo real, com entrada e remoção seguras.

Escopo:
- Grupo MLS por sala (`openmls`); novo epoch a cada entrada/saída; `Update` periódico — `SPEC-02` §4.1.
- Relay como Delivery Service: ordena commits por epoch — `SPEC-02` §4.2.
- Admissão com prova de `s` (HMAC sobre o KeyPackage) por um membro online — `SPEC-02` §3.3.
- **Remover membro** (dono: commit `Remove` + pedido assinado ao relay) e **sair da sala** — `RF-10`.
- Chat em grupo sobre stream QUIC confiável — `RF-05`.
- Presença (quem está na sala) e aviso de entrada — `RF-09` (parte texto).
- Histórico local opcional e cifrado. Sem mensagens offline (`SPEC-02` §8).

Sobre atalhos: uma chave simétrica de sala (sem MLS) pode existir **só como marco interno de desenvolvimento**, com a chave entregue cifrada por `s` e nunca passando pelo relay. Ela não passa no aceite desta fase, porque não faz rekey na remoção.

Estado: **implementada.** Chat na janela e no terminal (`--cli`), apelido opcional, histórico local opcional (desligado por padrão). Detalhes em `SPEC-02` §4.2.1. Testes: `crates/crypto/src/group.rs` (MLS) e `crates/host/tests/chat.rs` (ponta a ponta com relay real).

Aceite:
- [x] Mensagens cifradas; captura no relay é indecifrável. *O cifrado não contém o texto; o relay só guarda commits e nunca recebe chave do grupo.*
- [x] Relay que emite `t` sem conhecer `s` não consegue entrar no grupo. *Testado: o intruso entra no relay e fica "aguardando admissão" para sempre.*
- [x] Membro removido não lê mensagens posteriores (novo epoch) e tem a conexão derrubada.
- [x] Commits concorrentes no mesmo epoch são resolvidos sem divergência de estado. *Testado com três membros disputando a remoção de quem saiu; depois todos trocam mensagens.*
- [ ] Nenhum tráfego a domínios de terceiros. *Como na Fase 1: garantido pela configuração, falta captura de rede.*

Limitações conhecidas: o dono não pode sair (transferir a posse fica para depois); um membro não-dono consegue fazer commit de remoção de quem saiu (é o fluxo normal) e os clientes alertam se ele remover outra pessoa.

---

## Fase 3 — Voz
**Objetivo:** canal de voz em grupo, leve.

Escopo:
- Captura/saída WASAPI, Opus 24–32 kbps, VAD, WebRTC APM (AEC3 + NS + AGC), RNNoise opcional — `RF-06`, `SPEC-04` §1.
- Mídia em datagramas QUIC não-confiáveis + jitter buffer.
- SFrame com chave por remetente via MLS exporter; SFU roteia cego — `SPEC-02` §4.3.
- Controle de congestionamento: BBR + fila curta de datagramas + rate control na aplicação — `SPEC-03` §6.1.
- Presença de "falando agora" — `RF-09`.

Estado: **implementada; falta o teste de ouvido entre dois PCs.** Voz na janela (cartão "Voz": entrar, mudo, ensurdecer, supressão forte) e no terminal (`/voz`, `/mudo`, `/desmutar`, `/sair-da-voz`). Detalhes em `SPEC-04` §1.1, `SPEC-02` §4.3.1 e `SPEC-03` §6.1. Testes: `crates/media` (Opus, jitter, VAD, AEC, pipeline), `crates/crypto/src/sframe.rs` e `crates/host/tests/voice.rs` (ponta a ponta com relay real).

Aceite:
- [ ] Voz em grupo com < 150 ms de latência (mesma região). *Estimado em 60–90 ms + rede (`SPEC-04` §1.1). Falta medir boca-a-ouvido entre dois PCs.*
- [x] CPU < 2% em call de voz no Ryzen 5 5600. *Processo real, os dois lados falando o tempo todo: 0,26% da máquina (3,1% de um núcleo), 24 MB de RAM. Só o pipeline: 0,12%.*
- [x] SFU repassa voz sem decifrar. *O relay lê só o cabeçalho de roteamento e não tem chave. Testado: quem não está na voz não recebe, e a troca de epoch na remoção não interrompe quem fica.*
- [ ] Sem eco perceptível com caixa de som (sem fone). *AEC3 ligado; num eco sintético cai mais de 15 dB. Falta ouvir entre dois PCs com caixa de som.*

Limitações conhecidas: sem push-to-talk e sem escolha de dispositivo (usa o padrão do Windows); o binário chegou a 24,85 MiB, perto do limite de 25 (`SPEC-01` §10); na Fase 6 está em 22,8 MiB.

---

## Fase 4 — Compartilhamento de tela
**Objetivo:** o recurso-estrela, com máximo desempenho.

Escopo:
- Captura WGC/DXGI na GPU — `RF-07`, `SPEC-04` §2.
- Encode HW em cascata (AV1→HEVC→H.264→OpenH264). RX 6700 XT = HEVC/AMF.
- Negociação de codec pelo denominador comum de decode dos receptores.
- Encoding adaptativo: framerate guiado pelo WGC em tela estática, bitrate adaptativo com movimento, keyframe sob pedido.
- Simulcast 2–3 camadas (só modo servidor) + seleção por destinatário no SFU; AV1 SVC onde houver.
- Decode HW (D3D11VA) só do que está visível.

Estado: **implementada sem simulcast; falta o teste entre dois PCs.** Compartilhar e assistir na janela (cartão "Tela", botão "Assistir" na lista de membros, vídeo acima do chat) e no terminal (`/tela`, `/parar-tela`, `/assistir PREFIXO`, que decodifica sem mostrar e relata fps e banda). Quadros vão em **um stream QUIC por quadro**, não em datagramas (`SPEC-03` §6.3). Detalhes em `SPEC-04` §2.1. Testes: `crates/media/src/screen.rs` (selagem, reordenação, controle de bitrate), `crates/host/tests/screen.rs` (ponta a ponta com relay real, sem GPU) e `screen_roundtrip` (tela real → encoder → decoder → janela, com GPU e FFmpeg).

Aceite:
- [ ] Tela de código 1080p < 1 Mbps e < 3% de CPU no transmissor. *CPU: 0,09% da máquina (1,1% de um núcleo) transmitindo a 58 fps, processo real. Banda com a tela parada: falta medir (a tela de teste tinha animação o tempo todo).*
- [ ] Tela com jogo 1080p60 em 2–5 Mbps adaptativo. *Controle de bitrate entre 0,3 e 5 Mbps guiado pelos quadros em trânsito. Falta medir com jogo.*
- [x] Minimizar a transmissão libera o decoder (CPU cai). *"Fechar" encerra a thread de vídeo; janela principal minimizada faz o relay parar de mandar (volta num keyframe). Testado: quem para de assistir não recebe mais nenhum quadro.*
- [x] Encoder certo escolhido automaticamente por GPU. *Enumera os MFTs de hardware do Media Foundation. Na RX 6700 XT escolhe AMDh265Encoder / AMDh264Encoder. Só testado em AMD.*
- [ ] Receptor sem decode HEVC entra na sala e o transmissor desce de codec sozinho. *Implementado: cada membro anuncia o que decodifica pelo MLS, o transmissor renegocia a cada 250 ms e recria o encoder com keyframe. A negociação e os dois encoders (HEVC e H.264) estão testados; falta um receptor real sem HEVC.*

Fora desta entrega: simulcast e AV1 SVC (por enquanto o relay pula quadros para quem está atrasado e retoma no keyframe), OpenH264 por software (sem encoder de hardware não dá para compartilhar), compartilhar uma janela só (hoje é o monitor inteiro), várias telas assistidas ao mesmo tempo. O FFmpeg mínimo para distribuir entrou depois da Fase 6: `scripts/ffmpeg-min.sh` compila só `avcodec` + `avutil` com os decoders H.264/HEVC/AV1 e os hwaccels D3D11VA, sem bibliotecas externas, e a CI monta o zip (`lerrichat.exe` + `ffmpeg` + LEIA-ME) e, numa tag `v*`, a Release.

---

## Fase 5 — Câmera
**Objetivo:** vídeo de câmera reaproveitando o pipeline.

Escopo:
- Captura Media Foundation — `RF-08`.
- Mesmo caminho de encode/decode da tela; 720p30 ~1–1.5 Mbps.
- Tela + câmera simultâneas como streams independentes, respeitando o limite de sessões de encoder.

Estado: **implementada sem simulcast; falta o teste entre dois PCs.** Câmera na janela (cartão "Câmera" com escolha do dispositivo, botão "Câmera" na lista de membros, imagem ao lado da tela no painel de vídeo) e no terminal (`/camera`, `/parar-camera`, `/ver-camera PREFIXO`). O protocolo passou de "tela" para "stream de vídeo" (`Publish`, `Watch` e pedidos de keyframe com o tipo: tela ou câmera), então dá para publicar os dois e assistir a uma tela e uma câmera ao mesmo tempo. Detalhes em `SPEC-04` §3.1. Testes: `crates/host/tests/screen.rs` (tela e câmera juntas pelo relay real, cada quadro no destino certo) e `camera_roundtrip` (webcam real → encoder → decoder).

Aceite:
- [x] Câmera + tela ao mesmo tempo sem estourar as metas de CPU. *Processos reais, mesma máquina: quem publica tela (1080×1920 a 58 fps) e câmera (720p) gasta 0,17% da máquina (2% de um núcleo), 100 MB; quem assiste aos dois, 0,05% e 82 MB. Na primeira de três rodadas os dois streams pararam por ~5 s logo no início e voltaram sozinhos; não se repetiu.*
- [ ] Degradação graciosa por simulcast em rede ruim. *Sem simulcast ainda. No lugar, o relay pula quadros de quem está atrasado até o próximo keyframe, e a câmera desiste antes da tela (4 quadros em trânsito contra 8). Falta testar com rede ruim de verdade.*

O limite de sessões de encoder (NVENC de consumo) não chega a pesar: cada pessoa usa no máximo duas, tela e câmera.

---

## Fase 6 — UI e polimento
**Objetivo:** a casca visual (roxo/escuro) e a experiência leve.

Escopo:
- Telas na UI escolhida em D-02, seguindo `SPEC-05` (paleta `ink`/roxo do LerriNFC, P-01 resolvida).
- Fluxo completo: abrir → criar/entrar → voz/tela/câmera → sair.
- Indicadores de rede/latência, estados de "falando/compartilhando", aviso de entrada.
- Telas de remover membro e sair (a lógica já existe desde a Fase 2).
- Fricção do Windows: assinatura de código, explicação do firewall, tela "Sobre" com licenças — `SPEC-01` §9.

Estado: **implementada; falta o teste com um convidado leigo.** Sem assinatura de código (D-05). O que entrou:
- **Paleta do LerriNFC** (P-01) em todas as telas, com campo de texto, caixa de seleção e lista de escolha próprios: os do `std-widgets` usam o azul do Fluent e o Slint 1.18 não deixa trocar as cores do `Palette`.
- **Sala no layout do `SPEC-05` §4:** barra de controle embaixo (voz · tela · câmera) com linha de estado em texto, membros, detalhes da mídia e atividade à direita. Uma sala por vez, então a lista de salas fica na tela inicial em vez de numa barra lateral.
- **Indicador de rede** com caminho e latência; a cor segue a qualidade (bom < 120 ms e direto, atenção ≥ 120 ms ou pelo ponto de encontro, ruim ≥ 300 ms).
- **Remover e sair** com confirmação que explica a consequência (rekey e queda; sair apaga a sala do PC), e "Cancelar".
- **Firewall:** aviso no cartão "Criar sala x1", antes de o Windows perguntar.
- **Tela "Sobre"** com versão, o que o app faz na rede e as licenças de terceiros (inclui a atribuição do Slint exigida pela licença royalty-free).
- Botões, caixa de seleção e lista de escolha acessíveis por leitor de tela (papel, estado e ação padrão via UI Automation).
- **Ícone do app** (mascote) no `.exe` e na janela (`SPEC-05` §7).

Aceite:
- [x] RAM ociosa < 80 MB (RNF-01). *Release, processo real: 22 MB na tela inicial e 37 MB numa sala x1 parada hospedando a sala (working set).*
- [ ] Convidado leigo entra em < 60 s a partir do download, num Windows limpo. *Falta testar com uma pessoa. Sem assinatura de código, o SmartScreen ainda vai pedir "Mais informações → Executar assim mesmo".*
- [x] Telas só com tokens da paleta; todos os pares texto/fundo passam AA. *`crates/ui/tests/contrast.rs` lê o `global Theme` e confere cada par usado. Exceções: o QR (preto no branco) e a barra de rolagem padrão.*

Fora desta entrega: assinatura de código (D-05), anel de "falando" pulsante (é estático), indicador de latência por membro (só o caminho até o host) e push-to-talk.

---

## Fase 7 — Endurecimento (futuro)
- Verificação de peers (safety number) fora de banda.
- Fila cifrada de mensagens offline no relay (em avaliação).
- Empacotamento como serviço do Windows / unidade systemd no Proxmox.
- Linux/mac como cliente.

---

## Ordem de dependência

```
Fase 0 → Fase 1 → Fase 2 → Fase 3 → Fase 4 → Fase 5 → Fase 6 → Fase 7
(spikes) (rede)   (texto)   (voz)   (tela)   (câmera)  (UI)   (hardening)
```

A UI (Fase 6) é propositalmente tardia: é a camada mais isolada e a mais fácil de iterar depois que a engenharia pesada (rede + mídia) está de pé. Mas a **escolha** da tecnologia de UI acontece na Fase 0, porque depende do render de vídeo. Protótipos visuais podem andar em paralelo sem bloquear.
