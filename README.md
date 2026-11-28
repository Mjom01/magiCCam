# magiCCam

Pehelysúlyú (≈400 KB, telepítés és függőségek nélkül) Rust app, ami **bármilyen Windowsos videóforrást** teljes képernyőn, élő feedként mutat:

- USB / UVC webkamerák
- **OBS Virtual Camera**, NDI Webcam, NVIDIA Broadcast és más virtuális kamerák
- capture kártyák (Elgato, Magewell, AVerMedia, stb.)
- **Blackmagic ATEM Mini** (UVC webcam kimenet), DeckLink / UltraStudio / Intensity

## Hogyan működik

Két motorral dolgozik, eszközönként automatikusan választva:

**Hardveres eszközök** (USB/UVC kamerák, ATEM Mini, capture kártyák, NDI Webcam) →
**Media Foundation + Direct3D 11**, ugyanúgy, ahogy a Windows Kamera app:
- alacsony késleltetésű olvasás (`MF_LOW_LATENCY`), a tömörített képet (MJPEG/H.264) a
  videokártya dekódolja, ha a driver tud ilyet;
- a D3D11 videoprocesszor GPU-n konvertál, skáláz és letterboxol, egy flip-model swapchainre;
- sosem vár a vsync-re: ha a forrás gyorsabb a kijelzőnél, a régi képkocka kiesik, így késés
  nem tud felgyűlni. Mindig a legfrissebb kép látszik.

**Szoftveres források** (OBS Virtual Camera, NVIDIA Broadcast), **DeckLink**-szerű kártyák, és
minden, amit az MF nem tud megnyitni → **DirectShow**, mert csak az látja ezeket. Egy rövid tervet
jár végig (közvetlen GPU renderer → beszúrt dekóder/konverter → bármilyen renderer, minden
pixelformátummal), ellenőrzi, hogy a stream tényleg fut-e, és eszközönként megjegyzi, mi működött.

Formátumválasztás: folyamatos (≥24 fps) mód előnyben, max. 1080p (állítható), a nyers
formátum előnyben az MJPEG/H.264-gyel szemben. DeckLink-szerű kártyáknál a kártya beállított
formátumához nem nyúl.

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
magiccam --engine dshow      # mindig DirectShow (mf = mindig Media Foundation)
magiccam --clock on          # DirectShow: késő képkockák eldobása (legkisebb késleltetés)
magiccam --clock off         # DirectShow: minden képkocka kirajzolása (OBS-hez kell)
```

**Késleltetés a DirectShow-motorban:** a hardveres forrásoknál (USB-kamera, ATEM, capture kártya, DeckLink) a
renderer referenciaórával fut, így a késve érkező képkockákat eldobja, és mindig a legfrissebbet
mutatja. A szoftveres virtuális kameráknál (OBS, NVIDIA Broadcast) óra nélkül fut, mert azok
időbélyegeihez egyik óra sem illeszkedik. Ezt az app magától eldönti (`--clock auto`), de felülírható.

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
