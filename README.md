# magiCCam

Pehelysúlyú (≈220 KB, nulla függőség a Windows API-n kívül) Rust app, ami **bármilyen Windowsos videóforrást** teljes képernyőn, élő feedként mutat:

- USB / UVC webkamerák
- **OBS Virtual Camera**, NDI Webcam, NVIDIA Broadcast és más virtuális kamerák
- capture kártyák (Elgato, Magewell, AVerMedia, stb.)
- **Blackmagic ATEM Mini** (UVC webcam kimenet), DeckLink / UltraStudio / Intensity

## Hogyan működik

DirectShow-t használ, mert ez az egyetlen Windows API, ami *minden* fenti forrást lát
(az OBS virtuális kamera és a DeckLink is csak itt jelenik meg). A kép a forrásból
közvetlenül az **Enhanced Video Renderer**-be (GPU-s skálázás, letterbox) megy, a program
egyetlen képkockát sem másol — ezért gyenge gépen is alig terhel. Nagyon régi rendszeren
automatikusan a régi DirectShow rendererre vált vissza.

Formátumválasztás: folyamatos (≥24 fps) mód előnyben, max. 1080p (állítható), a nyers
formátum előnyben az MJPEG/H.264-gyel szemben (kevesebb CPU). DeckLink-szerű kártyáknál
a kártya beállított formátumához nem nyúl.

Ha a forrás leesik (kihúzzák, OBS leáll), 2 másodpercenként újrapróbálja és magától visszajön.
Futás közben nem engedi elaludni a kijelzőt.

## Használat

```
magiccam                     # utoljára használt (vagy első) kamera, teljes képernyő
magiccam --list              # eszközlista
magiccam -d "ATEM"           # eszköz névrészlet vagy sorszám alapján
magiccam -d 2 -m 2           # 2. eszköz a 2. monitoron
magiccam -w                  # ablakban indul
magiccam --max-height 720    # max. 720p automatikus választásnál (0 = nincs limit)
magiccam -k                  # ne állítson formátumot
```

| Billentyű | Funkció |
|---|---|
| Esc / Q | kilépés |
| F / F11 / Enter / dupla klikk | teljes képernyő ki/be |
| Tab, nyilak, Space, 1–9 | kamera váltás |
| M | következő monitor |
| R | eszközök újraolvasása |
| jobb klikk | menü (eszközlista, stb.) |

## Build

```
cargo build --release
```
Kimenet: `target/release/magiccam.exe` (Windows 7+, x64).
