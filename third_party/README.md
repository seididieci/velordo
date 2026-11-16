# Font di terze parti

## MapleMono-NF-Regular.ttf

- **Progetto**: Maple Mono (https://github.com/subframe7536/maple-font), nella
  variante con i glifi Nerd Fonts ("NF").
- **Licenza**: SIL Open Font License 1.1 — testo completo in `LICENSE.txt`.
- **Uso** (D3, ADR-0060): il font del terminale grafico. Il driver carica
  il file dal volume root (`/font.ttf`, oggetto ArcaFS `sys:font.ttf`) e lo
  rasterizza **a runtime** in `libs/term` (punto fisso, nessun float): nel
  repo c'e' solo il TTF, nessun atlas pre-generato.
- **Attribuzione** (OFL 1.1): Maple Mono Project Authors
  (https://github.com/subframe7536/maple-font). Il file e' ridistribuito
  invariato, con la licenza qui accanto.

Le scelte di progetto (dimensione del font, griglia, cella) sono in
`docs/src/adr/0060-console-virtuale-driver-display.md`.

## ca/DigiCertGlobalRootG3.der

- **Cos'e'**: la **root** `DigiCert Global Root G3` (X.509 DER, 579 byte),
  che firma l'intermediate `DigiCert Global G3 TLS ECC SHA384 2020 CA1` usato
  dalle catene ECDSA di DigiCert.
- **Provenienza**: scaricata dal repository pubblico DigiCert
  (`https://cacerts.digicert.com/DigiCertGlobalRootG3.crt`), ridistribuita
  invariata. Non scade prima del 2038.
- **Uso** (N6.3, ADR-0072): trust anchor `sys:ca.der` per provare il DoT
  contro un resolver pubblico (es. Quad9 `2620:fe::fe:853`). `embedded-tls`
  verifica l'**ultimo** certificato inviato dal server contro l'anchor:
  Quad9 invia `[leaf, intermediate]`, quindi l'anchor deve essere la root,
  non l'intermediate. Seed:
  `TLS_TEST_CA=third_party/ca/DigiCertGlobalRootG3.der ...`.
- **Licenza**: un certificato pubblico non e' soggetto a copyright nel
  contenuto-chiave; la ridistribuzione nei trust store e' prassi comune.

