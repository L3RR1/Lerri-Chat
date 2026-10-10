# Lerri Chat

> App de voz/chat/compartilhamento de tela **self-hosted**, **anônimo** e **focado em desempenho** para Windows. Um único `.exe`, sem instalação, sem conta, sem nuvem de terceiros.

## O que é

Um "Discord" mínimo e brutalmente leve:

- **Um `.exe`** que roda como **cliente** ou como **host** (você escolhe no primeiro uso).
- **Salas por código de convite único.** Sem diretório público, sem busca, sem listagem.
- **Servidor cego:** o servidor (relay) repassa pacotes criptografados ponta a ponta (E2EE). Ele nunca vê mensagens, voz, tela ou câmera — e não consegue se colocar dentro da sala.
- **Anônimo:** sua identidade é um par de chaves gerado no seu PC. Sem e-mail, telefone ou nome.
- **Só o essencial:** chat de texto em grupo, voz, compartilhamento de tela e câmera. Nada de loja, nitro, telemetria, atividade ou animação supérflua.
- **Desempenho acima de tudo:** UI nativa (sem navegador embutido), captura e codificação de vídeo na GPU, decodificação só do que você está olhando.

## Dois modos de host (você decide)

| Modo | Quem hospeda | Quando usar |
|------|--------------|-------------|
| **x1 (P2P direto)** | Um dos participantes vira host; o outro conecta direto nele | 2 pessoas, latência mínima, zero infra se o PC de quem cria for alcançável (IPv6 ou porta mapeada) |
| **Servidor (SFU)** | Uma máquina dedicada (VPS ou Proxmox de casa) repassa pra todos | 3+ pessoas, quem fala não some quando o host fecha o PC |

A mesma sala pode nascer em qualquer um dos modos — a escolha é do usuário na criação.

## Como ler este repositório

Desenvolvimento guiado por especificação (SDD). Primeiro a spec, depois o código. Ordem de leitura:

1. [`SPEC-00-visao.md`](SPEC-00-visao.md) — requisitos e princípios
2. [`SPEC-01-arquitetura.md`](SPEC-01-arquitetura.md) — stack e desenho técnico
3. [`SPEC-02-seguranca.md`](SPEC-02-seguranca.md) — identidade, E2EE, convites, modelo de ameaça
4. [`SPEC-03-rede.md`](SPEC-03-rede.md) — QUIC, NAT, CGNAT, x1 vs servidor
5. [`SPEC-04-midia.md`](SPEC-04-midia.md) — voz, tela e câmera com máximo desempenho
6. [`SPEC-05-design-system.md`](SPEC-05-design-system.md) — a identidade visual (roxo/escuro)
7. [`SPEC-06-fases.md`](SPEC-06-fases.md) — roadmap por fases com critérios de aceite

## Estado

- **Fase 0 — Fundação, concluída.** Workspace, identidade local cifrada e os spikes que fecharam as decisões D-01 a D-03 (ver [`SPEC-06`](SPEC-06-fases.md), [`spikes/video`](spikes/video/README.md) e [`spikes/p2p`](spikes/p2p/README.md)).
- **Fase 1 — Salas, convites e conectividade: implementada, falta o teste com redes reais.** Salas nos dois modos, convites de uso único com QR, links `lerrichat://`, x1 direto e x1 assistido pelo ponto de encontro do seu relay. Já tem uma interface mínima (início e sala); a interface completa é a Fase 6.

## Compilar e testar

Requer Rust stable e, no Windows, as Build Tools do Visual Studio (MSVC + Windows SDK).

```sh
cargo test --workspace --exclude spike-video --exclude spike-p2p
cargo build --release -p lerrichat          # target/release/lerrichat.exe
cargo build --release -p lerrichat-host     # relay headless (Linux)
```

## Usar

Abra o `lerrichat.exe`: a janela tem **Entrar numa sala** (cole o convite), **Criar sala x1**, **Minhas salas** e **Criar sala num servidor**. Na sala aparecem o convite (link + QR, botão de copiar), quem está online, o caminho da conexão com a latência e a atividade. Na primeira vez que abre, ele registra os links `lerrichat://` para o seu usuário do Windows, então clicar num convite já abre a sala.

Pela linha de comando (relay, testes):

Sala x1, sem servidor (dois PCs):

```sh
lerrichat --x1                       # PC de quem cria: endereços, aviso de alcance e convite no terminal
lerrichat --cli --join "lerrichat://join?…"  # PC do convidado (sem --cli, abre a janela)
```

Sala num servidor (Proxmox/VPS):

```sh
lerrichat-host --public <IP ou domínio>   # no servidor; mostra o link de administração e o link público
lerrichat --create "lerrichat://admin?…"   # no seu PC: cria a sala e mostra o convite
lerrichat --open <id>                      # volta a uma sala salva
```

x1 assistido (você tem um relay, mas a sala é direta entre os dois PCs):

```sh
lerrichat --x1 --via "lerrichat://relay?…"
```

`lerrichat --register` faz os links `lerrichat://` abrirem o programa. Dentro de uma sala: `convite`, `membros`, `rota`, `sair`.

O relay usa a porta `P` em UDP (salas), TCP (ponto de encontro) e `P+1` em UDP (descoberta de endereço). Atrás de um roteador, abra as três.

## Escopo de uso

Ferramenta de comunicação privada entre pessoas que trocam o código de convite por fora. Sem moderação central porque não há central. O dono de cada sala é responsável por quem convida.
