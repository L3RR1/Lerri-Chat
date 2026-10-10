# SPEC-06 — Roadmap por Fases (SDD)

Status: **Draft** · Depende de: todas as specs anteriores

Desenvolvimento guiado por especificação: cada fase tem escopo fechado, critérios de aceite verificáveis e só começa quando a anterior passa. Nada de pular pra mídia antes da fundação de rede funcionar.

## Decisões e pendências abertas

| ID | Decisão | Onde se decide |
|----|---------|----------------|
| D-01 | NAT traversal: **iroh** com relay próprio × implementação própria sobre `quinn` (`SPEC-03` §4.4) | Spike da Fase 0 |
| D-02 | UI: **Slint** × `egui` + `wgpu`, conforme o render de vídeo sem cópia (`SPEC-01` §10) | Spike da Fase 0 |
| D-03 | Decode: FFmpeg mínimo (D3D11VA) × MFT de hardware (`SPEC-01` §1) | Spike da Fase 0 |
| P-01 | Casar os hex de roxo com o `Color.kt` do LerriNFC (`SPEC-05`) | Antes da Fase 6 |

---

## Fase 0 — Fundação + spikes de risco
**Objetivo:** esqueleto do projeto, conexão QUIC viva e os três riscos técnicos que podem mudar a stack respondidos **antes** de escrever código de produto.

Escopo:
- Workspace Rust com os crates de `SPEC-01` (`core`, `crypto`, `net`, `media`, `host`, `client`, `ui`, `app`, `host-bin`).
- Geração de identidade local (par de chaves Ed25519) cifrada com DPAPI — `RF-01`.
- Conexão QUIC cliente↔relay com TLS 1.3 (`quinn`), echo de mensagem de teste.
- CI de build do `lerrichat.exe` (Windows) e do `lerrichat-host` (Linux), medindo o tamanho do binário.
- **Spike D-02/D-03:** decodificar um stream HEVC 1080p60 com D3D11VA e exibir na UI sem cópia pra CPU.
- **Spike D-01:** dois peers atrás de NAT conectando via iroh com relay próprio (sem relays da n0).

Fora de escopo: UI real, convites, chat.

Aceite:
- [x] `lerrichat.exe` e `lerrichat.exe --host` sobem e trocam uma mensagem por QUIC (IPv4 e IPv6; certificado do relay fixado por fingerprint). *`lerrichat-host` no Linux: compila na CI, ainda não rodado num servidor real.*
- [x] Identidade persiste cifrada (DPAPI) entre reinícios.
- [x] Binário < 25 MB (RNF-02), partida < 1,5 s (RNF-09). *Medido sem UI: 2,7 MB e ~30 ms. Remedir após D-02.*
- [ ] Spike de vídeo: 1080p60 decodificado e exibido com CPU < 5% → D-02 e D-03 registradas.
- [ ] Spike de rede: resultado registrado em D-01.

---

## Fase 1 — Salas, convites e conectividade
**Objetivo:** criar sala, gerar e consumir convites únicos, e conectar nos dois modos.

Escopo:
- `RoomId`, `RoomConfig`, papéis Dono/Membro/Relay — `RF-02`, `SPEC-02` §2.
- Convites de uso único com `t` (roteamento) e `s` (admissão) + QR — `RF-03`, `SPEC-02` §3. Nesta fase o relay valida e consome `t`; a verificação de `s` entra na Fase 2, junto com o MLS.
- Entrada colando o link ou abrindo o `lerrichat://` (handler em HKCU) — `RF-04`.
- Conectividade x1 sem infra: IPv6 + PCP/NAT-PMP/UPnP + aviso de criador inalcançável (`SPEC-03` §4.2).
- Conectividade x1 assistida: STUN embutido no relay, sinalização, hole punching e relay de último caso (`SPEC-03` §4.3).

Aceite:
- [ ] Criar sala nos dois modos.
- [ ] Reuso de convite é recusado.
- [ ] x1 sem infra conecta quando o criador tem IPv6 ou porta mapeada; avisa na criação quando não tem.
- [ ] x1 assistido: dois peers em NATs comuns conectam por hole punching, com mídia direta.
- [ ] Cliente alcança o SFU no Proxmox com porta UDP aberta ou via IPv6.
- [ ] Nenhum STUN/relay de terceiros é contatado.

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

Aceite:
- [ ] Mensagens cifradas; captura no relay é indecifrável.
- [ ] Relay que emite `t` sem conhecer `s` não consegue entrar no grupo.
- [ ] Membro removido não lê mensagens posteriores (novo epoch) e tem a conexão derrubada.
- [ ] Commits concorrentes no mesmo epoch são resolvidos sem divergência de estado.
- [ ] Nenhum tráfego a domínios de terceiros.

---

## Fase 3 — Voz
**Objetivo:** canal de voz em grupo, leve.

Escopo:
- Captura/saída WASAPI, Opus 24–32 kbps, VAD, WebRTC APM (AEC3 + NS + AGC), RNNoise opcional — `RF-06`, `SPEC-04` §1.
- Mídia em datagramas QUIC não-confiáveis + jitter buffer.
- SFrame com chave por remetente via MLS exporter; SFU roteia cego — `SPEC-02` §4.3.
- Controle de congestionamento: BBR + fila curta de datagramas + rate control na aplicação — `SPEC-03` §6.1.
- Presença de "falando agora" — `RF-09`.

Aceite:
- [ ] Voz em grupo com < 150 ms de latência (mesma região).
- [ ] CPU < 2% em call de voz no Ryzen 5 5600.
- [ ] SFU repassa voz sem decifrar.
- [ ] Sem eco perceptível com caixa de som (sem fone).

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

Aceite:
- [ ] Tela de código 1080p < 1 Mbps e < 3% de CPU no transmissor.
- [ ] Tela com jogo 1080p60 em 2–5 Mbps adaptativo.
- [ ] Minimizar a transmissão libera o decoder (CPU cai).
- [ ] Encoder certo escolhido automaticamente por GPU.
- [ ] Receptor sem decode HEVC entra na sala e o transmissor desce de codec sozinho.

---

## Fase 5 — Câmera
**Objetivo:** vídeo de câmera reaproveitando o pipeline.

Escopo:
- Captura Media Foundation — `RF-08`.
- Mesmo caminho de encode/decode da tela; 720p30 ~1–1.5 Mbps.
- Tela + câmera simultâneas como streams independentes, respeitando o limite de sessões de encoder.

Aceite:
- [ ] Câmera + tela ao mesmo tempo sem estourar as metas de CPU.
- [ ] Degradação graciosa por simulcast em rede ruim.

---

## Fase 6 — UI e polimento
**Objetivo:** a casca visual (roxo/escuro) e a experiência leve.

Escopo:
- Telas na UI escolhida em D-02, seguindo `SPEC-05` (paleta `ink`/roxo do LerriNFC, P-01 resolvida).
- Fluxo completo: abrir → criar/entrar → voz/tela/câmera → sair.
- Indicadores de rede/latência, estados de "falando/compartilhando", aviso de entrada.
- Telas de remover membro e sair (a lógica já existe desde a Fase 2).
- Fricção do Windows: assinatura de código, explicação do firewall, tela "Sobre" com licenças — `SPEC-01` §9.

Aceite:
- [ ] RAM ociosa < 80 MB (RNF-01).
- [ ] Convidado leigo entra em < 60 s a partir do download, num Windows limpo.
- [ ] Telas só com tokens da paleta; todos os pares texto/fundo passam AA.

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
