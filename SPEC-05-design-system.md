# SPEC-05 — Design System

Status: **Draft** · Fase: 0 · Depende de: SPEC-00

A interface segue a **mesma linha espiritual do LerriNFC**: fundo escuro quase-preto com viés arroxeado, acento roxo vivo, estética "lab/ferramenta" enxuta.

> **Pendente (P-01):** casar os hex de roxo com o `Color.kt` do LerriNFC. A paleta abaixo está ancorada no fundo real do LerriNFC (`ink = #0B0710`) e usa roxos coerentes com o ícone. Ao trocar qualquer hex, refazer a tabela de contraste do §8.

## 1. Princípios visuais

1. **Escuro de verdade.** Fundo quase-preto arroxeado; nada de cinza "dark mode genérico".
2. **Roxo é o acento, não o preenchimento.** Usado em estados ativos, foco, "quem está falando", botões primários. O resto respira no escuro.
3. **Densidade calma.** Ferramenta, não rede social. Pouca cor, pouco ruído, zero animação supérflua (coerente com o princípio de desempenho).
4. **Nativo.** Renderizado por Slint na GPU. A estética não pode custar desempenho.

## 2. Paleta

### Base (escuros)
| Token | Hex | Uso |
|-------|-----|-----|
| `ink` | `#0B0710` | Fundo raiz (igual ao LerriNFC) |
| `surface` | `#140D1F` | Painéis, barra lateral |
| `surface-raised` | `#1D1430` | Cards, campos, modais |
| `surface-hover` | `#271A42` | Hover/seleção |
| `border` | `#2E2142` | Divisórias sutis |

### Acento (roxo) — *ajustar ao Color.kt*
| Token | Hex | Uso |
|-------|-----|-----|
| `violet` | `#A855F7` | Primário: fundo de botões, foco, bordas, badges |
| `violet-bright` | `#C084FC` | Hover do primário, **links e texto roxo sobre superfícies** |
| `violet-deep` | `#7C3AED` | Pressionado, sombra de realce |
| `violet-glow` | `rgba(168,85,247,.25)` | Halo de "falando agora", foco |

### Texto
| Token | Hex | Uso |
|-------|-----|-----|
| `text` | `#F4F0FA` | Texto principal |
| `text-dim` | `#B3A7C9` | Secundário, labels |
| `text-placeholder` | `#8E83A8` | Placeholder de campos |
| `text-mute` | `#6E6384` | Só desabilitado (isento de contraste mínimo) |

### Semânticos
| Token | Hex | Uso |
|-------|-----|-----|
| `success` | `#34D399` | Conectado, online (lembra o verde dos olhos do ícone) |
| `warning` | `#FBBF24` | Rede instável, atenção |
| `danger` | `#F87171` | Erro, desconectar, remover |

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

## 5. Componentes

- **Botão primário:** fundo `violet`, texto `ink`, hover `violet-bright`.
- **Botão fantasma:** borda `border`, texto `text`, hover `surface-hover`.
- **Campo de código de convite:** mono, fundo `surface-raised`, com botão "copiar" e QR ao lado.
- **Tile de participante:** avatar/inicial, nick, anel `violet-glow` quando está falando.
- **Indicador de rede:** ponto `success`/`warning`/`danger` + latência em ms (pra ferramenta que preza desempenho, mostrar o número é identidade).
- **Toast:** canto inferior, `surface-raised`, borda `violet`.

## 6. Estados que merecem destaque

- **Falando agora:** anel pulsante sutil em `violet-glow` no tile (única animação "viva" permitida). Ela força redesenho enquanto alguém fala, então: redesenha **só o tile**, no máximo a 30 fps, e para assim que o VAD indica silêncio. Com "reduzir animações" do Windows ativo, vira anel estático.
- **Alguém entrou na sala:** aviso discreto no chat com o apelido e o início do `MemberId` (exigência de segurança, `SPEC-02` §7).
- **Compartilhando tela:** badge `violet` no tile + ícone de tela.
- **Degradação de rede:** o número de latência muda de cor; sem modal intrusivo.

## 7. Ícone / marca

- Herdar a linguagem do ícone do LerriNFC (criatura roxa, olhos verdes, fundo preto) como família visual, sem copiar o mesmo desenho.
- App icon: monograma/símbolo roxo sobre `ink`.

## 8. Acessibilidade

Contraste mínimo **WCAG AA** (4,5:1 para texto normal). Pares calculados com a paleta atual:

| Texto | Fundo | Contraste | Status |
|-------|-------|-----------|--------|
| `text` | `ink` | 17,8:1 | ✅ |
| `text-dim` | `surface` | 8,4:1 | ✅ |
| `text-dim` | `surface-raised` | 7,8:1 | ✅ |
| `text-dim` | `surface-hover` | 7,1:1 | ✅ |
| `ink` (botão primário) | `violet` | 5,0:1 | ✅ |
| `violet-bright` (link) | `surface-raised` | 6,7:1 | ✅ |
| `text-placeholder` | `surface-raised` | 5,0:1 | ✅ |
| `violet` como texto | `surface-raised` | 4,4:1 | ❌ não usar `violet` pra texto — usar `violet-bright` |
| `text-mute` | `surface` | 3,4:1 | ⚠️ só pra desabilitado |

- Estados nunca dependem só de cor (ícone + cor juntos).
- Alvos de toque/click ≥ 32 px.
- Respeitar a configuração "reduzir animações" do Windows.

## 9. Critérios de aceite

- [ ] Telas principais usando só tokens desta paleta.
- [ ] Hex de roxo casados com o `Color.kt` do LerriNFC (P-01), com a tabela de contraste refeita.
- [ ] Todos os pares texto/fundo usados nas telas passam AA.
- [ ] Nenhuma animação contínua além do anel de "falando" (orçamento de desempenho).
