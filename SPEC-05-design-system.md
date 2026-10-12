# SPEC-05 — Design System

Status: **Implementado na Fase 6, refinado depois dela** · Depende de: SPEC-00

A interface segue a **mesma linha espiritual do LerriNFC**: fundo escuro quase-preto com viés arroxeado, acento roxo vivo, estética "lab/ferramenta" enxuta.

> **P-01 resolvida:** todos os hex vêm do `Color.kt` do LerriNFC (`app/src/main/java/com/lerri/nfclab/ui/theme/Color.kt`). Os tokens moram no `global Theme` de `crates/ui/ui/app.slint`, e `crates/ui/tests/contrast.rs` lê de lá e recalcula a tabela do §8: trocar um hex sem passar AA quebra o teste.

## 1. Princípios visuais

1. **Escuro de verdade.** Fundo quase-preto arroxeado; nada de cinza "dark mode genérico".
2. **Roxo é o acento, não o preenchimento.** Usado em estados ativos, foco, "quem está falando", botões primários. O resto respira no escuro.
3. **Densidade calma.** Ferramenta, não rede social. Pouca cor, pouco ruído. Movimento só para mostrar mudança de estado (hover, foco, aba), em 120–180 ms.
4. **Nativo e leve.** UI no renderer **software** do Slint (D-02): sem driver gráfico, ~22 MB ociosa. A estética não pode custar desempenho: nada de sombras pesadas, blur ou gradientes animados. Vídeo fica numa área própria (janela-filha D3D11) e a UI não desenha por cima dela.
5. **Uma tarefa lidera cada tela.** Na inicial, entrar com um convite; na sala, conversar. O resto fica em volta, mais quieto, e o raro (servidor, ponto de encontro, endereços, ajustes de áudio) fica a um clique.
6. **Separar com espaço, não com caixas.** Blocos se distinguem por espaço e, quando preciso, uma linha fina; cartão com borda só onde o conteúdo é de fato um objeto à parte (aviso, confirmação). Painéis que abrem sob demanda substituem o conteúdo lateral em vez de empilhar.

## 2. Paleta

### Base (escuros)
| Token | Hex | `Color.kt` | Uso |
|-------|-----|-------------|-----|
| `ink` | `#0B0710` | `Ink` | Fundo raiz |
| `surface` | `#150E1F` | `Surface1` | Painéis, cartões, barra de controle |
| `surface-raised` | `#1C1329` | `Surface2` | Campos, listas, botões desabilitados |
| `surface-hover` | `#271A38` | `Surface3` | Hover/seleção |
| `border` | `#342447` | `Outline` | Divisórias sutis |

### Acento (roxo)
| Token | Hex | `Color.kt` | Uso |
|-------|-----|-------------|-----|
| `violet` | `#B36BFF` | `Accent` | Primário: fundo de botões, foco, bordas |
| `violet-bright` | `#CB9BFF` | `Violet` | Hover do primário, **texto roxo sobre superfícies** (você no chat, "falando", "dono") |
| `violet-deep` | `#8E7BFF` | `Blue` | Pressionado |
| `violet-dim` | `#2E1A47` | `AccentDim` | Fundo do botão tonal e do avatar |
| `violet-dim-hover` | `#3A2159` | — | Hover do botão tonal, seleção de texto |
| `on-violet` | `#1A0A2E` | `OnAccent` | Texto sobre `violet`/`violet-bright`/`violet-deep` |
| `violet-glow` | `#B36BFF40` | `Accent` a 25% | Anel de "falando agora" |

### Texto
| Token | Hex | `Color.kt` | Uso |
|-------|-----|-------------|-----|
| `text` | `#F2ECFB` | `TextHigh` | Texto principal |
| `text-dim` | `#B4A3C9` | `TextMid` | Secundário, labels, horários, detalhes técnicos |
| `text-placeholder` | `#8E83A8` | — | Placeholder de campos (o `Color.kt` não tem; o `TextLow` não passa AA) |
| `text-mute` | `#766288` | `TextLow` | Só desabilitado e ponto de offline (isento de contraste mínimo) |

### Semânticos
| Token | Hex | `Color.kt` | Uso |
|-------|-----|-------------|-----|
| `success` | `#3DDC97` | `Success` | Conectado, online, rota boa |
| `warning` | `#FFB454` | `Warning` | Rede instável, aguardando admissão, atenção |
| `danger` | `#FF5A6E` | `Danger` | Erro, sair, remover |
| `success-dim` / `warning-dim` / `danger-dim` | `#0F3327` / `#3A2A14` / `#3A1620` | `*Dim` | Fundo das faixas de aviso e de erro |

## 3. Tipografia

- **UI:** **Segoe UI Variable** (fonte do sistema no Windows 11; Segoe UI no Windows 10). Nenhuma fonte embutida — embutir a Inter custaria megabytes do orçamento de 25 MB (RNF-02).
- **Mono:** **Cascadia Mono** (vem com o Windows Terminal; fallback Consolas) para códigos de convite e IDs.
- Escala (razão ~1,2): 12 (detalhe) · 13 (rótulo de campo, botão pequeno) · 14 (corpo) · 15 (título de painel) · 16–18 (título de tela/sala) · 24 (a tarefa principal da tela inicial).
- Peso: 400 corpo, 600 títulos e botões, 700 só na inicial do avatar. Nada de light em fundo escuro (ilegível).
- Mono só onde o conteúdo é código: convite, endereços, ids.

**Cantos:** controles 8 px, painéis 12 px, avatares inteiros (tokens `r-control` e `r-panel`).

## 4. Layout

```
┌───────────┬──────────────────────────────────────┐
│ salas     │  cabeçalho da sala · membros · status │
│ (ícones)  ├──────────────────────────────────────┤
│           │                                      │
│  [sala]   │   área principal                      │
│  [sala]   │   (vídeo/tela em grade, ou chat)      │
│           │                                      │
│           ├──────────────────────────────────────┤
│  você     │  barra de voz: mic · tela · cam · sair │
└───────────┴──────────────────────────────────────┘
```

- **Barra lateral estreita** com as salas (como o Discord, mas enxuta).
- **Área principal** alterna entre grade de vídeo/tela e chat.
- **Barra de controle** fixa embaixo: microfone, compartilhar tela, câmera, sair. Ícones grandes, estados claros.

Como ficou: o app abre **uma sala por vez**, então no lugar da barra lateral de salas há a lista "Minhas salas" na tela inicial.

- **Inicial:** barra do topo só com o nome e "Sobre". Uma coluna centrada: "Entrar numa sala" lidera, com o campo do convite e o apelido logo abaixo; uma linha separa "Criar uma sala", com "Criar sala x1" e "No meu servidor" lado a lado (o link de administração só aparece depois do clique) e o ponto de encontro recolhido. "Minhas salas" só aparece quando há alguma; o id fica no "Sobre". Rola quando a janela fica baixa.
- **Espera:** o que está acontecendo e uma barra indeterminada.
- **Sala:** topo com o tipo ("Sala x1" / "Sala no servidor"), o id discreto e uma linha de estado só (cadeado, ponto e texto do caminho de rede, papel); à direita, Convidar, Desconectar (ou **Encerrar sala**, para o dono de uma x1, porque a sala vive no PC dele) e Sair da sala. A coluna principal é a conversa, sem moldura: avisos, vídeos assistidos e o chat, com avatar, mensagens seguidas da mesma pessoa agrupadas e um estado vazio que explica a cifragem (sozinho na sala, ele chama para o convite, com "Convidar alguém"). A prévia da própria câmera entra na grade de vídeos como "Sua câmera". Você aparece como "Você" (ou "apelido (você)"), e o apelido se troca ali mesmo na lista. À direita, um **painel de contexto** que mostra uma coisa por vez: "Na sala" (avatares, presença, ações por membro e a atividade embaixo), "Convidar" (QR, link, copiar, gerar outro, endereços sob demanda) ou "Voz e áudio" (dispositivos, modo de transmitir, supressão e detalhes da mídia). Abrir um deles nunca empurra a conversa nem a barra. Embaixo, a barra de controle numa linha: voz | tela + o que compartilhar | câmera + qual câmera à esquerda, o estado em texto à direita. Abaixo de 1000 px de largura, os botões ficam só com ícone.
- **Sobre:** coluna centrada com versão, o seu id, rede/firewall e licenças em lista.

## 5. Componentes

- **Botão (`Btn`)**, com ícone, texto ou os dois, em três alturas (30, 36, 44):
  - **primário**: fundo `violet`, texto `on-violet`, hover `violet-bright`, pressionado `violet-deep`. Uma ação principal por área (Entrar, Enviar, Copiar convite, Entrar na voz).
  - **tonal**: fundo `violet-dim`, texto `violet-bright`. A segunda ação importante (Criar sala, Convidar, Compartilhar tela, Assistir).
  - **fantasma**: borda `border`, texto `text`, hover `surface-hover`.
  - **perigo**: borda e texto `danger`.
  - **ligado**: o mesmo botão em `warning`, para alternâncias (microfone mudo, ensurdecido).
- **Ícones:** Tabler Icons 3.47.0 (MIT), traço de 2 px, rasterizados em PNG de 48 px e pintados com `colorize`.
- **Campos, caixa de seleção, abas e menu de dispositivo:** componentes próprios (`Field`, `Check`, `Segmented`, `DeviceMenu`), fundo `surface-raised`, borda `violet` no foco. Os do `std-widgets` usam o azul de destaque do Fluent, e o Slint 1.18 não deixa trocar as cores do `Palette`. Rótulo sempre acima do campo; o placeholder é só exemplo.
- **Linha de estado:** ícone ou ponto colorido mais texto curto, separados por "·", no topo da sala e à direita da barra de controle. O estado nunca é só cor.
- **Faixa (`Banner`):** aviso dentro da tela, com ícone, texto e as ações (ex.: "Cancelar" e "Sair de vez").
- **Campo de código de convite:** mono, fundo `surface-raised`, com botão "copiar" e QR ao lado (o QR fica preto no branco, como os leitores esperam).
- **Tile de participante:** avatar/inicial, nick, latência até a pessoa (com a cor do indicador de rede) e, quando ela fala, ondas roxas saindo do avatar e uma forma de onda no lugar do ícone de microfone.
- **Indicador de rede:** ponto `success`/`warning`/`danger` + caminho + latência em ms (pra ferramenta que preza desempenho, mostrar o número é identidade). Bom: direto e < 120 ms; atenção: ≥ 120 ms ou pelo ponto de encontro; ruim: ≥ 300 ms (aí a voz passa dos 150 ms boca-a-ouvido).
- **Confirmações:** em linha, no lugar onde a ação aconteceu (remover membro, sair da sala), nunca em modal.

## 6. Estados que merecem destaque

- **Movimento:** transições de 120–180 ms em fundo e borda (hover, foco, aba). O único laço é a barra da tela de espera, que dura poucos segundos.

- **Falando agora:** duas ondas roxas (`violet-bright`) saem do avatar, crescem e somem em sequência, e cinco barras ao lado do nome sobem e descem em senoides defasadas (única animação "viva"). Um relógio só, de 30 fps, na sala, ligado só enquanto alguém fala: custa 0,5% de um núcleo (seguindo a taxa do monitor, 165 Hz, eram 45%). Com os efeitos de animação do Windows desligados, vira anel e barras parados.
- **Alguém entrou na sala:** aviso discreto no chat com o apelido e o início do `MemberId` (exigência de segurança, `SPEC-02` §7).
- **Compartilhando tela:** badge `violet` no tile + ícone de tela.
- **Degradação de rede:** o número de latência muda de cor; sem modal intrusivo.

## 7. Ícone / marca

- Herdar a linguagem do ícone do LerriNFC (criatura roxa, olhos verdes, fundo preto) como família visual, sem copiar o mesmo desenho.
- App icon: o mascote roxo de olhos verdes (arte `LCOREMASCOTE`, sem o texto "CHAT", que vira borrão em tamanho pequeno). `app/assets/lerrichat.ico` vai embutido no `.exe` com 16–256 px: de 48 px para cima o desenho inteiro, até 32 px só a cabeça, para os olhos continuarem legíveis. A janela usa `crates/ui/ui/icon.png` (cabeça, 64 px), porque só aparece pequena (título, barra de tarefas). O mascote não aparece dentro do app: as telas ficam só com o conteúdo.

## 8. Acessibilidade

Contraste mínimo **WCAG AA** (4,5:1 para texto normal). Pares usados nas telas, calculados por `crates/ui/tests/contrast.rs`:

| Texto | Fundo | Contraste |
|-------|-------|-----------|
| `text` | `ink` / `surface` / `surface-raised` / `surface-hover` | 17,3 / 16,3 / 15,5 / 14,1 |
| `text` | `danger-dim` / `warning-dim` (faixas) / `violet-dim` | 13,8 / 11,9 / 13,4 |
| `violet-bright` | `violet-dim` / `violet-dim-hover` (botão tonal) | 7,1 / 6,3 |
| `text-dim` | `ink` / `surface` / `surface-raised` / `surface-hover` | 8,6 / 8,1 / 7,7 / 7,0 |
| `text-placeholder` | `surface-raised` | 5,1 |
| `on-violet` | `violet` / `violet-bright` / `violet-deep` | 5,7 / 8,6 / 5,7 |
| `violet-bright` | `surface` / `surface-raised` / `surface-hover` | 8,7 / 8,2 / 7,5 |
| `success` | `surface` / `surface-raised` | 10,7 / 10,1 |
| `warning` | `surface` / `surface-raised` / `warning-dim` | 10,7 / 10,1 / 7,8 |
| `danger` | `ink` / `surface` / `surface-raised` / `surface-hover` / `danger-dim` | 6,6 / 6,2 / 5,9 / 5,4 / 5,3 |
| `text-mute` | `surface` | 3,5 — ⚠️ só desabilitado |

- Estados nunca dependem só de cor: voz, tela, câmera e rede têm texto ("Voz: mudo", "Tela no ar · 2 assistindo", "Caminho direto · 3 ms").
- Alvos de clique ≥ 30 px nos botões pequenos, 36–44 px no resto.
- Teclado: botões e caixa de seleção entram no Tab e respondem a Enter/Espaço, com anel `violet-bright` só no foco por teclado (o clique não deixa anel).
- Botões, campos, caixa de seleção, abas e menus expõem papel, estado, nome e ação padrão para leitores de tela (UI Automation). Botão só de ícone tem nome próprio ("Silenciar microfone"). Os campos aceitam escrita por acessibilidade (ditado, leitores de tela).
- O movimento é curto e só em mudança de estado; nada pisca nem se repete na sala.

## 9. Critérios de aceite

- [x] Telas principais usando só tokens desta paleta. *Exceções: o QR (preto no branco) e a barra de rolagem do `ScrollView` padrão.*
- [x] Hex de roxo casados com o `Color.kt` do LerriNFC (P-01), com a tabela de contraste refeita.
- [x] Todos os pares texto/fundo usados nas telas passam AA. *Conferido por teste.*
- [x] Nenhuma animação contínua na sala além das ondas de quem está falando (orçamento de desempenho). *30 fps, só enquanto alguém fala, 0,5% de um núcleo.*
- [x] Cada tela tem uma tarefa principal clara, estado vazio que ensina e confirmações em linha.
- [x] Navegável por teclado, com foco visível.
