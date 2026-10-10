# SPEC-00 — Visão e Requisitos

Status: **Draft** · Fase: 0 · Depende de: nada

## 1. Problema

Ferramentas de voz/chat atuais (Discord à frente) são pesadas: cada uma é um Chromium inteiro, consome 400 MB+ de RAM, roda telemetria e trava em PCs modestos. Além disso, dependem de servidores de terceiros que veem metadados e conteúdo. Queremos o oposto: leve ao extremo, self-hosted, e cego ao conteúdo.

## 2. Visão em uma frase

> Um único `.exe` que te dá voz, chat e compartilhamento de tela/câmera com amigos, onde o servidor não consegue ler nada e o app quase não pesa na máquina.

## 3. Princípios (em ordem de prioridade)

1. **Desempenho primeiro.** Toda decisão de design perde para "isso deixa o app mais leve/rápido?". Meta de RAM ociosa: **< 80 MB**. Meta de uso de CPU em call de voz: **< 2%**. Tela parada compartilhada: **< 1 Mbps e < 3% CPU** no transmissor.
2. **Cego ao conteúdo.** O servidor (relay) nunca tem acesso a texto, voz, tela ou câmera em claro, nem consegue se colocar dentro da sala. E2EE sempre, sem modo "desligado".
3. **Anônimo por padrão.** Sem conta, e-mail, telefone ou nome real. Identidade = par de chaves local.
4. **Zero fricção.** Baixou o `.exe`, abriu, colou o código, está dentro. Sem instalador, sem dependências externas obrigatórias.
5. **Mínimo viável de recursos.** Só: texto em grupo, voz, tela, câmera. Tudo além disso é rejeitado por padrão.

## 4. Requisitos funcionais

| ID | Requisito |
|----|-----------|
| RF-01 | Gerar identidade local (par de chaves) no primeiro uso, sem nenhum dado pessoal. |
| RF-02 | Criar uma sala, escolhendo o modo de host: **x1 (P2P)** ou **servidor**. |
| RF-03 | Gerar códigos de convite **únicos e de uso único** para uma sala. |
| RF-04 | Entrar em uma sala colando um código de convite. |
| RF-05 | Chat de texto em grupo, em tempo real, E2EE. |
| RF-06 | Canal de voz em grupo, E2EE, com supressão de ruído. |
| RF-07 | Compartilhar tela (monitor ou janela) com alta eficiência. |
| RF-08 | Compartilhar câmera. |
| RF-09 | Ver quem está na sala e quem está falando/transmitindo (presença). |
| RF-10 | Sair da sala e revogar a própria presença; o **dono** da sala pode remover membro (papéis em `SPEC-02` §2). |

## 5. Requisitos não-funcionais

| ID | Requisito | Meta |
|----|-----------|------|
| RNF-01 | RAM ociosa do cliente | < 80 MB |
| RNF-02 | Tamanho do `.exe` | < 25 MB, sem runtime externo |
| RNF-03 | CPU em call de voz (sem vídeo) | < 2% em Ryzen 5 5600 |
| RNF-04 | Banda de tela estática (1080p, código/docs) | 0,3–1 Mbps |
| RNF-05 | Banda de tela com movimento (1080p60, jogo) | 2–5 Mbps, adaptativo |
| RNF-06 | Latência de voz boca-a-ouvido (mesma região) | < 150 ms |
| RNF-07 | Latência adicional do relay (modo servidor) | < 30 ms sobre o RTT direto |
| RNF-08 | Uso de CPU no transmissor de tela (captura + encode na GPU) | < 3% em Ryzen 5 5600 |
| RNF-09 | Partida a frio do app | < 1,5 s até a tela de sala |

## 6. Modos de host

O usuário escolhe na criação da sala. Detalhes de rede em `SPEC-03`.

- **Modo x1 (P2P direto):** um participante abre a sala e vira host. O segundo conecta direto. Sem intermediário. Ideal pra 2 pessoas. Se o host fecha, a sala acaba. Sem nenhuma infra, exige que o criador seja alcançável (IPv6 ou porta mapeada); senão, um servidor do próprio usuário serve de ponto de encontro (`SPEC-03` §4).
- **Modo servidor (SFU):** uma máquina dedicada (VPS barata ou um container no Proxmox de casa) roda o relay em modo serviço. Repassa mídia pra todos (Selective Forwarding Unit). Aguenta 3–10 pessoas e sobrevive a qualquer cliente sair.

No modo servidor, o relay é **cego**: só vê pacotes cifrados e não consegue se admitir na sala. No x1, o host é o próprio participante criador.

## 7. Não-objetivos (explicitamente fora)

- Contas, perfis, avatares, amigos persistentes.
- Servidores públicos, diretório, descoberta.
- Mobile (fase futura, não agora).
- Bots, integrações, webhooks, loja.
- Histórico de longo prazo no servidor (o servidor não guarda nada legível).
- Esconder o IP dos participantes entre si / do host (ver modelo de ameaça em `SPEC-02`).

## 8. Personas

- **Host técnico (Lerri):** roda o servidor no Proxmox ou numa VPS, distribui o `.exe` e os convites.
- **Convidado:** baixa o `.exe`, cola o código, usa. Não sabe nem precisa saber de rede.

## 9. Critérios de aceite da visão

- [ ] Um convidado leigo entra numa sala em < 60 s a partir do download.
- [ ] Wireshark no relay mostra apenas tráfego cifrado indecifrável.
- [ ] Gerenciador de Tarefas mostra o cliente ocioso abaixo de 80 MB.
- [ ] Compartilhar uma tela de código a 1080p fica abaixo de 1 Mbps.
