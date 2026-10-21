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
