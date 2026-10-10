# Spike D-01 — iroh com relay próprio

Pergunta: dá para usar o **iroh** (QUIC + hole punching + relay) sem nenhum
servidor da n0, só com o relay do próprio usuário? (`SPEC-03` §4.4)

## Como rodar

```sh
cargo run --release -p spike-p2p
```

Sobe um relay iroh local com certificado autoassinado, dois endpoints
configurados com `presets::Minimal` + `RelayMode::Custom` (só o relay local) e
faz o peer B conectar no A **conhecendo apenas o id de A e a URL do relay**,
como seria num convite.

## Resultado (2026-10-09, Windows 10, mesma máquina)

```
relay local: https://localhost:49572/ (QUIC na porta 49300)
B conectou em 31.5622ms
caminhos (logo após conectar):
  relay  Relay(https://localhost:49572/) rtt 22.2109ms ← em uso
eco por stream: 9.246ms
caminhos (após 148.3477ms):
  relay  Relay(https://localhost:49572/) rtt 14.229712ms
  direto Ip([2804:…]:49303) rtt 1.315437ms ← em uso
eco por stream (caminho atual): 632.3µs
tamanho máximo de datagrama: Some(1414)
datagramas: 1000/1000 voltaram (0.0% de perda local)
```

- **Conecta só pelo relay próprio** e, em ~150 ms, migra sozinho para o
  caminho direto (aqui, IPv6). Esse é exatamente o "x1 assistido" da SPEC-03 §4.3.
- **Datagramas funcionam** (máx. 1414 bytes por datagrama), que é o que a mídia usa.
- **Nenhum terceiro no código usado:** os relays e o DNS da n0 só entram com
  `RelayMode::Default`/`presets::N0`. As referências a Cloudflare DNS estão só
  em testes. *Verificado por leitura do código-fonte, não por captura de rede.*
- O **mapeamento de porta no roteador (PCP/NAT-PMP/UPnP)** já vem no iroh
  (feature `portmapper`), coincidindo com a SPEC-03 §4.2.
- **Tamanho:** o binário de release do spike (cliente **e** servidor de relay)
  tem 7,4 MB. 394 crates contra 107 do `lerrichat` atual.

## O que muda se adotarmos

- **O iroh não usa o `quinn`**, usa um fork próprio (`noq`). O crate `net`
  passa a usar a API do iroh em vez do `quinn` direto.
- **O relay do iroh fala HTTPS/WebSocket (TCP)** quando repassa tráfego. Só é
  usado se o caminho direto falhar; nesse caso, a mídia passa por TCP
  (aceitável como último recurso; o caminho feliz continua UDP).
- **Identidade de transporte do iroh = chave Ed25519.** Não reutilizar a chave
  de identidade do membro: o relay veria o mesmo id em todas as salas. Usar uma
  chave de transporte por sala/sessão.
- O relay do Lerri Chat passa a embutir o `iroh-relay` (feature `server`) junto
  do SFU.

## O que este spike NÃO provou

- **NAT de verdade.** Tudo rodou numa máquina só. O teste com dois PCs em redes
  diferentes (incluindo CGNAT) fica no aceite da Fase 1.
- Ausência de tráfego para terceiros **medida na rede** (Wireshark).
