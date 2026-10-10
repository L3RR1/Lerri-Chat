# SPEC-05 — Design System

Status: **Implementado na Fase 6** · Depende de: SPEC-00

A interface segue a **mesma linha espiritual do LerriNFC**: fundo escuro quase-preto com viés arroxeado, acento roxo vivo, estética "lab/ferramenta" enxuta.

> **P-01 resolvida:** todos os hex vêm do `Color.kt` do LerriNFC (`app/src/main/java/com/lerri/nfclab/ui/theme/Color.kt`). Os tokens moram no `global Theme` de `crates/ui/ui/app.slint`, e `crates/ui/tests/contrast.rs` lê de lá e recalcula a tabela do §8: trocar um hex sem passar AA quebra o teste.

## 1. Princípios visuais

1. **Escuro de verdade.** Fundo quase-preto arroxeado; nada de cinza "dark mode genérico".
2. **Roxo é o acento, não o preenchimento.** Usado em estados ativos, foco, "quem está falando", botões primários. O resto respira no escuro.
3. **Densidade calma.** Ferramenta, não rede social. Pouca cor, pouco ruído, zero animação supérflua (coerente com o princípio de desempenho).
4. **Nativo e leve.** UI no renderer **software** do Slint (D-02): sem driver gráfico, ~22 MB ociosa. A estética não pode custar desempenho: nada de sombras pesadas, blur ou gradientes animados. Vídeo fica numa área própria (janela-filha D3D11) e a UI não desenha por cima dela.

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
| `violet-dim` | `#2E1A47` | `AccentDim` | Seleção de texto |
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
- Escala: 12 (micro) · 14 (corpo) · 16 (ênfase) · 20 (título de painel) · 28 (título de tela).
- Peso: 400 corpo, 600 títulos. Nada de light em fundo escuro (ilegível).

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

Como ficou na Fase 6: o app abre **uma sala por vez**, então no lugar da barra lateral de salas há a lista "Minhas salas" na tela inicial. A sala tem cabeçalho (nome, convite, desconectar/sair) e indicador de rede; à esquerda, convite, vídeo assistido e chat; à direita, membros, detalhes da mídia e atividade; embaixo, a barra de controle (voz · tela · câmera), com botões de texto em vez de ícones (nenhuma fonte de ícones embutida, §3) e uma linha de estado. Tela "Sobre" com versão, rede/firewall e licenças.

## 5. Componentes

- **Botão primário:** fundo `violet`, texto `on-violet`, hover `violet-bright`, pressionado `violet-deep`.
- **Botão fantasma:** borda `border`, texto `text`, hover `surface-hover`.
- **Campos, caixa de seleção e lista de escolha:** componentes próprios (`Field`, `Check`, `Picker`), fundo `surface-raised`, borda `violet` no foco. Os do `std-widgets` usam o azul de destaque do Fluent, e o Slint 1.18 não deixa trocar as cores do `Palette`.
- **Campo de código de convite:** mono, fundo `surface-raised`, com botão "copiar" e QR ao lado (o QR fica preto no branco, como os leitores esperam).
- **Tile de participante:** avatar/inicial, nick, anel `violet-glow` quando está falando.
- **Indicador de rede:** ponto `success`/`warning`/`danger` + caminho + latência em ms (pra ferramenta que preza desempenho, mostrar o número é identidade). Bom: direto e < 120 ms; atenção: ≥ 120 ms ou pelo ponto de encontro; ruim: ≥ 300 ms (aí a voz passa dos 150 ms boca-a-ouvido).
- **Toast:** canto inferior, `surface-raised`, borda `violet`.

## 6. Estados que merecem destaque

- **Falando agora:** anel pulsante sutil em `violet-glow` no tile (única animação "viva" permitida). Ela força redesenho enquanto alguém fala, então: redesenha **só o tile**, no máximo a 30 fps, e para assim que o VAD indica silêncio. Com "reduzir animações" do Windows ativo, vira anel estático.
- **Alguém entrou na sala:** aviso discreto no chat com o apelido e o início do `MemberId` (exigência de segurança, `SPEC-02` §7).
- **Compartilhando tela:** badge `violet` no tile + ícone de tela.
- **Degradação de rede:** o número de latência muda de cor; sem modal intrusivo.

## 7. Ícone / marca

- Herdar a linguagem do ícone do LerriNFC (criatura roxa, olhos verdes, fundo preto) como família visual, sem copiar o mesmo desenho.
- App icon: o mascote roxo de olhos verdes (arte `LCOREMASCOTE`, sem o texto "CHAT", que vira borrão em tamanho pequeno). `app/assets/lerrichat.ico` vai embutido no `.exe` com 16–256 px: de 48 px para cima o desenho inteiro, até 32 px só a cabeça, para os olhos continuarem legíveis. A janela usa `crates/ui/ui/icon.png` (cabeça, 64 px), porque só aparece pequena (título, barra de tarefas).

## 8. Acessibilidade

Contraste mínimo **WCAG AA** (4,5:1 para texto normal). Pares usados nas telas, calculados por `crates/ui/tests/contrast.rs`:

| Texto | Fundo | Contraste |
|-------|-------|-----------|
| `text` | `ink` / `surface` / `surface-raised` / `surface-hover` | 17,3 / 16,3 / 15,5 / 14,1 |
| `text` | `danger-dim` (faixa de erro) / `violet-dim` (seleção) | 13,8 / 13,4 |
| `text-dim` | `ink` / `surface` / `surface-raised` / `surface-hover` | 8,6 / 8,1 / 7,7 / 7,0 |
| `text-placeholder` | `surface-raised` | 5,1 |
| `on-violet` | `violet` / `violet-bright` / `violet-deep` | 5,7 / 8,6 / 5,7 |
| `violet-bright` | `surface` / `surface-raised` / `surface-hover` | 8,7 / 8,2 / 7,5 |
| `success` | `surface` / `surface-raised` | 10,7 / 10,1 |
| `warning` | `surface` / `surface-raised` / `warning-dim` | 10,7 / 10,1 / 7,8 |
| `danger` | `ink` / `surface` / `surface-raised` / `surface-hover` | 6,6 / 6,2 / 5,9 / 5,4 |
| `text-mute` | `surface` | 3,5 — ⚠️ só desabilitado |

- Estados nunca dependem só de cor: voz, tela, câmera e rede têm texto ("Voz: mudo", "Tela no ar · 2 assistindo", "Caminho direto · 3 ms").
- Alvos de clique ≥ 26 px nos botões pequenos da lista de membros, 36 px no resto.
- Botões, caixa de seleção e lista de escolha expõem papel, estado e ação padrão para leitores de tela (UI Automation).
- Nenhuma animação, então "reduzir animações" já é respeitado.

## 9. Critérios de aceite

- [x] Telas principais usando só tokens desta paleta. *Exceções: o QR (preto no branco) e a barra de rolagem do `ScrollView` padrão.*
- [x] Hex de roxo casados com o `Color.kt` do LerriNFC (P-01), com a tabela de contraste refeita.
- [x] Todos os pares texto/fundo usados nas telas passam AA. *Conferido por teste.*
- [x] Nenhuma animação contínua além do anel de "falando" (orçamento de desempenho). *Nem ele anima: é um anel estático.*
