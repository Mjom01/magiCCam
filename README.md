# magiCCam

Pehelysúlyú (≈220 KB, nulla függőség a Windows API-n kívül) Rust app, ami **bármilyen Windowsos videóforrást** teljes képernyőn, élő feedként mutat:

- USB / UVC webkamerák
- **OBS Virtual Camera**, NDI Webcam, NVIDIA Broadcast és más virtuális kamerák
- capture kártyák (Elgato, Magewell, AVerMedia, stb.)
- **Blackmagic ATEM Mini** (UVC webcam kimenet), DeckLink / UltraStudio / Intensity

## Hogyan működik

DirectShow-t használ, mert ez az egyetlen Windows API, ami *minden* fenti forrást lát
(az OBS virtuális kamera és a DeckLink is csak itt jelenik meg). A kép GPU-s rendererbe
(EVR, ill. VMR-9) megy, ami skáláz és letterboxol — gyenge gépen is alig terhel.

Megnyitáskor egy rövid tervet jár végig, és az első működő kombinációt használja:

1. forrás → GPU renderer **közvetlenül** (nulla konverzió — a hardveres kamerák/kártyák így mennek)
2. ugyanez VMR-9-cel (régebbi gépek)
3. DirectShow által beszúrt dekóderrel/konverterrel (MJPEG kamerák, illetve a virtuális kamerák,
   pl. OBS, NVIDIA Broadcast, amik nem hajlandók közvetlenül GPU-pufferbe írni)
4. bármilyen renderer (végső tartalék)

Minden lépésnél kipróbálja ugyanannak a módnak (felbontás + fps) a többi pixelformátumát is
(NV12 / I420 / YUY2 / MJPEG...), és ellenőrzi, hogy a stream tényleg fut-e (pl. az OBS NV12-je
csatlakozik, de azonnal leáll — ilyenkor továbblép). A működő kombinációt eszközönként megjegyzi,
így az újracsatlakozás azonnali.

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

## Hibakeresés

`MAGICCAM_DEBUG=1` környezeti változóval a `%TEMP%\magiccam_debug.txt`-be naplózza, melyik
renderer / formátum kombinációt próbálta, mi lett a graph, és 3 mp után a kirajzolt kép átlagos
fényerejét (0 = fekete kép).

```
$env:MAGICCAM_DEBUG=1; .\magiccam.exe -w
```

## Build

```
cargo build --release
```
Kimenet: `target/release/magiccam.exe` (Windows 10/11, x64). A C runtime statikusan van belefordítva, így nem kell hozzá VC++ Redistributable.
