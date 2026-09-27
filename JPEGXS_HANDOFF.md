
# ALVR JPEG XS Side-Channel — Handoff / Session Summary

Stand: 2026-08-22. Für Fortsetzung in Claude Code ("Code"-Tab). Repo: `C:\Temp\ALVR-org\ALVR-v20` (ALVR-Fork). SVT-JPEG-XS-Quellcode (separat geklont): `C:\Temp\SVT-JPEG-XS`.

## 1. Was das Feature ist

Ein **test-only, additiver Seitenkanal-Encoder**: Neben dem echten HEVC/H.264/AV1-Stream, den ALVR ans Headset schickt, läuft optional ein zweiter JPEG-XS-Encoder (Intel SVT-JPEG-XS, BSD-2-Clause-Patent) mit, der dasselbe GPU-Frame bekommt, es aber nur zu Vergleichszwecken auf die Festplatte dumpt. Der echte Stream zum Headset ist davon nie betroffen.

Kernklasse: `alvr/server_openvr/cpp/alvr_server/JpegXsSideEncoder.h/.cpp` (kompiliert nur wenn `ALVR_JPEGXS` definiert ist, was `build.rs` automatisch setzt, sobald `deps/windows/svt-jpegxs/{include,lib}` vorhanden sind — siehe `deps/windows/svt-jpegxs/README.md`).

## 2. Geänderte/neue Dateien (git status)

Neu:
- `alvr/server_openvr/cpp/alvr_server/JpegXsSideEncoder.h`
- `alvr/server_openvr/cpp/alvr_server/JpegXsSideEncoder.cpp`
- `alvr/server_openvr/cpp/alvr_server/StreamDumper.h`

Geändert:
- `alvr/server_core/src/connection.rs`
- `alvr/server_openvr/build.rs`
- `alvr/server_openvr/cpp/alvr_server/Settings.cpp` / `.h`
- `alvr/server_openvr/cpp/platform/win32/CEncoder.cpp` / `.h`
- `alvr/server_openvr/cpp/platform/win32/VideoEncoderAMF.cpp` / `.h`
- `alvr/server_openvr/cpp/platform/win32/VideoEncoderNVENC.cpp` / `.h`
- `alvr/server_openvr/cpp/platform/win32/d3d-render-utils/RenderUtils.h`
- `alvr/session/src/lib.rs`
- `alvr/session/src/settings.rs`
- `alvr/xtask/src/build.rs`

(Unwichtig/Artefakt: `alvr/server_openvr/test.o`, kann gelöscht werden.)

## 3. Dashboard-Settings (alles unter Video → Encoder → "Jpeg Xs")

In `alvr/session/src/settings.rs`, `JpegXsConfig`:

| Feld | Dashboard-Label | Zweck |
|---|---|---|
| `enabled` | Enable JPEG XS side-channel test | Ein/Aus |
| `bits_per_pixel` | Bits per pixel | Ziel-bpp, Default 6.0 |
| `thread_count` | Encoder thread count | 0 = auto (halbe Kernzahl) |
| `dump_to_disk` | Dump stream to disk | JPEG-XS-Rohstream + Stats-CSV nach Output directory |
| `dump_reference_stream` | Dump reference stream (HEVC/H.264/AV1) to disk | Steuert jetzt auch den *echten* Stream-Dump (vorher hart codiert als `kStreamDumpEnabled = true` in `StreamDumper.h`) |
| `output_directory` | Output directory | Default `C:\Temp` (Windows) |

Fluss: `settings.rs` → `lib.rs` (`OpenvrConfig` flache Felder, Präfix `jpeg_xs_*`) → `connection.rs` (`construct_openvr_config`) → C++ `Settings.h/.cpp` (Präfix `m_jpegXs*`) → `JpegXsSideEncoder.cpp` liest `Settings::Instance()`.

Alle Felder sind `steamvr-restart`-geflaggt.

## 4. Build-Tooling-Erleichterungen

- `cargo xtask build-streamer --release --keep-config` — `--keep-config` (existierendes Flag) verhindert, dass `session.json` beim Rebuild überschrieben wird.
- `alvr/xtask/src/build.rs`: kopiert jetzt automatisch `deps/windows/svt-jpegxs/bin/SvtJpegxs.dll` in den Build-Output (`bin/win64`), kein manuelles Kopieren mehr nötig.

## 5. Die "grüner Bildschirm"-Debugging-Saga (chronologisch, gelöst)

Ausgangspunkt: JPEG-XS-Aufnahme war zunächst "grünlich", nach einem ersten Rewrite (Zwei-Pass-Renderer statt ALVRs `RenderPipelineYUV`) komplett grün. Über ~10 Iterationen eingegrenzt:

1. **State-Reset, Textur-Format-Wechsel (NV12→getrennte Y/UV-Texturen), Flush(), IA-Layout-Fixes** — alle notwendig, aber nicht hinreichend.
2. **Kern-Ursache #1 (SRV/RTV-Hazard)**: `sourceTexture` war zum Zeitpunkt unseres `PSSetShaderResources`-Aufrufs noch als Render-Target auf dem geteilten Immediate-Context gebunden (von ALVRs eigenem `FrameRender`). D3D11 setzt SRV-Bindings bei einem Read/Write-Hazard **still** auf `nullptr` zurück — kein Fehler, kein Log, einfach leere Texture-Reads. Fix: `context->OMSetRenderTargets(1, {nullptr}, nullptr)` **vor** dem SRV-Bind. Bestätigt via Pre-Draw-Diagnose (`OMGetRenderTargets`/`PSGetShaderResources`-Vergleich), die `srv=MISMATCH` auf jedem Frame zeigte.
3. **Kern-Ursache #2 (UV-Viewport)**: Der Y- und UV-Pass teilten sich ein volles Viewport, obwohl das UV-Render-Target nur halb so groß ist (Trick aus ALVRs eigenem `rgbtoyuv420.hlsl`, der auf einer echten NV12-Planar-RTV mit Treiber-Auto-Scaling beruht — funktioniert nicht mit zwei gewöhnlichen, getrennten Texturen). Fix: eigene, zur Laufzeit per `D3DCompile` kompilierte Y/UV-Shader (kein `*2 mod width`-Trick mehr, einfaches direktes `uv`-Sampling) + echtes halbgroßes Viewport für den UV-Pass.

Ergebnis: Bild ist korrekt sichtbar, keine Grünstiche mehr.

## 6. Zwei Nachfolge-Bugs (beide gelöst)

- **Bild zu dunkel (sRGB-Gamma-Bug)**: `sourceTexture` ist `DXGI_FORMAT_R8G8B8A8_UNORM_SRGB` (voll typisiert, nicht `TYPELESS`). `Sample()` linearisiert automatisch (Gamma-Dekodierung), aber die BT.709-Koeffizienten erwarten Gamma-kodierte Werte. Ein Versuch, das SRV-Format explizit zu überschreiben, schlug fehl (D3D11 erlaubt keine andere View-Formatierung auf einer voll typisierten, nicht-typeless Ressource — das war Ursache eines nachfolgenden Abstürzes, siehe unten). Finaler Fix: **Re-Encode im Shader** — ein `offset.w`-Flag im Constant Buffer (host-seitig gesetzt, wenn die Quelltextur `_SRGB` ist) steuert eine branchless sRGB-Re-Encodierung nach dem `Sample()`-Aufruf, bevor die BT.709-Koeffizienten angewendet werden.
- **`OK_OR_THROW`-Makro-Bug** (`d3d-render-utils/RenderUtils.h`): formatierte die (immer narrow `const char*`) `msg` mit `%ls` statt `%hs` — führte bei jedem tatsächlichen Fehlschlag zu einer leeren/kaputten Exception-Meldung. Fiel erst auf, als der SRV-Format-Override-Versuch (s.o.) tatsächlich fehlschlug und die Fehlermeldung leer war. Gefixt auf `%hs`.

## 7. Performance-Untersuchung (Speed-Tests, teils noch offen)

Beobachtung: Bei mehreren Testläufen mit Half-Life Alyx brach die fps (sowohl im echten HEVC-Stream **als auch** im JPEG-XS-Seitenkanal) zeitweise stark ein, korreliert mit dem verzögerten Spielstart/Laden.

- **Bitrate-Rechnung** (bestätigt aus CSV-Logs): bpp(6.0) × 4288×1664 Luma-Samples × 90fps ≈ **3,85 Gbit/s** Ziel-Bitrate für den reinen JPEG-XS-Rohdump — das entspricht ~481 MB/s Schreibdurchsatz nach `C:\Temp`.
- **Test 1** (JPEG XS an, inkl. Disk-Dump): schwerer Einbruch (~10fps über mehrere Sekunden), sowohl im echten als auch im JPEG-XS-Stream.
- **Test 2** (JPEG XS aus): nur kurze, milde Einbrüche (~2-4s) im echten Stream.
- **Test 3** (JPEG XS an, aber `dump_to_disk` aus): **längerer** Einbruch (~35s, 60-70fps) als Test 2, aber nicht so extrem wie Test 1. → Festplatten-I/O ist **ein** Faktor, aber nicht der einzige.
- **Offene Hypothese (nicht verifiziert)**: CPU-Thermal-Throttling durch die 12 sustained SVT-JPEG-XS-Encoder-Threads (Einbruch beginnt erst nach ~60s Last — klassisches Throttling-Timing-Muster, nicht ein einmaliger Lade-Spike). Nächster Schritt: HWiNFO64/Task-Manager-Taktraten parallel zum Test beobachten, oder Thread-Count testweise auf 4-6 reduzieren.

**Nächster konkreter Test (nicht durchgeführt)**: Thread-Count auf z.B. 6 reduzieren UND/ODER Taktraten während des Laufs live beobachten, um Throttling zu bestätigen/widerlegen.

## 8. Apple Vision Pro / WebXR / WASM-Recherche (Diskussion, kein Code)

Frage war: Könnte man den gespeicherten JPEG-XS-Test-Stream in Safari auf der Vision Pro (M2 oder M5) abspielen, via WebAssembly + WebGPU + WebXR?

**Ergebnis der Recherche:**
- Kein Hardware-JPEG-XS-Decoder in Apples VideoToolbox (M2 oder M5) — müsste Software-Decode sein.
- JPEG XS ist explizit für Low-Complexity-Realtime-Software-Decode designt (intoPIX FastTicoXS bewirbt bereits Echtzeit-4K/8K-Decode auf ARM/Apple M1) — Chancen stehen prinzipiell gut.
- Foveated Encoding: der Decoder müsste nur die **übertragene** Auflösung (4288×1664) stemmen, nicht die native Panel-Auflösung — das "Entzerren" auf Display-Auflösung ist ein separater, günstiger Post-Processing-Schritt.
- Plattform-Voraussetzungen sind erfüllt: Safari 26 auf visionOS unterstützt WebGPU; Safari seit visionOS 2 / Safari 18 unterstützt WebXR immersive-vr; **seit Safari 26.2 unterstützt WebXR auch das WebGPU-Binding** (`XRGPUBinding`) — die komplette Kette (WASM-Decode → WebGPU-Compute für Wavelet-Rekonstruktion/Farbkonvertierung → Stereo-Ausgabe via WebXR+WebGPU) ist plattformseitig möglich.
- Kein fertiger JPEG-XS-WASM-Decoder existiert (nur JPEG XL-Decoder wie jSquash/JXL.js — anderer, unverwandter Codec).

**SVT-JPEG-XS-Quellcode-Analyse** (`C:\Temp\SVT-JPEG-XS`, jetzt als Cowork-Ordner verbunden):
- Der Decoder folgt dem "generischer C-Referenzcode + optionale x86-SIMD" Muster (rtcd = Runtime CPU Detection).
- **Wichtiger Fund**: `Source/Lib/Decoder/Codec/decoder_dsp_rtcd.c` — die komplette x86-SIMD-Dispatch-Logik (`SET_FUNCTIONS_X86`) ist hinter `#ifdef ARCH_X86_64` versteckt. Ohne dieses Define fällt alles automatisch auf die portable C-Implementierung zurück.
- Relevante Dateien für einen Emscripten-Port: `Source/Lib/Common/Codec/*.c` + `Source/Lib/Decoder/Codec/*.c` (die `ASM_AVX2`/`ASM_AVX512`/`ASM_SSE4_1`/`ASM_SSE2`-Unterordner **weglassen**, kein NASM/YASM nötig).
- **Blocker**: Top-Level-`CMakeLists.txt` geht hart von x86_64 aus (`enable_language(ASM_NASM)`, `add_definitions(-DARCH_X86_64=1)` unbedingt) — kann nicht direkt mit `emcmake` durchgereicht werden, braucht eigenes schlankes Build-Setup.
- **Konnte nicht weiterverfolgt werden**: Die Cowork-Sandbox hat kein Emscripten installiert und kein Root/apt-Zugriff, das Netzwerk ist auf eine kleine Allowlist beschränkt (GitHub, npm-Registry, jsdelivr etc. alle blockiert, 403). Kompilieren/Testen war dort nicht möglich.

**Entscheidung**: Projekt vorerst zurückgestellt (User-Entscheidung). Bei Bedarf fortsetzen mit: eigenem minimalem CMakeLists/emcc-Kommando (nur die genannten Dateien, ohne `ARCH_X86_64`-Define) + schlankem C-Wrapper (ein Frame decodieren → planare YUV-Daten), das lokal (z.B. in Claude Code mit echtem Dateisystem-/Toolchain-Zugriff) gebaut und getestet werden müsste — kein Weg, das in der Cowork-Sandbox zu verifizieren.

## 9. Offene Punkte / Nächste Schritte

- [ ] Task #10 (aus der ursprünglichen Liste): Test bei normaler Auflösung, 90fps-Parität bestätigen — noch offen.
- [ ] CPU-Throttling-Hypothese verifizieren (Taktraten während Last live beobachten, oder Thread-Count reduzieren und Effekt vergleichen).
- [x] WASM/WebGPU-Decode-Prototyp (siehe Abschnitt 10) — **funktioniert**, lokal gebaut und im Browser verifiziert (2026-08-22).
- [ ] Render-Loop von `setTimeout` zurück auf `requestAnimationFrame` umstellen für den produktiven/immer-sichtbaren Fall (Vision Pro Safari) — siehe Abschnitt 10.
- [ ] Echtes Live-Streaming (statt statischer Datei) an den WASM-Decoder anbinden — z. B. WebTransport/WebSocket-Quelle statt `fetch()`.
- [ ] WebXR-Integration (stereo Ausgabe) — siehe Abschnitt 8, plattformseitig möglich, noch nicht implementiert.
- [ ] Auf echter Vision-Pro-Hardware (Safari) verifizieren, ob COOP/COEP-Header + SharedArrayBuffer/pthreads dort wie erwartet funktionieren.
- [ ] `alvr/server_openvr/test.o` aufräumen (Artefakt, nicht versioniert).

## 10. WASM/WebGPU-Decode-Prototyp — funktioniert (2026-08-22)

Lokal in Claude Code (nicht Cowork-Sandbox) fortgesetzt, wie in Abschnitt 8 als nächster Schritt vorgesehen. Ziel: JPEG-XS-Stream im Browser dekodieren und anzeigen (WASM + WebGPU), 90fps als Zielgröße; WebXR/Live-Stream-Anbindung bewusst zurückgestellt.

**Ergebnis: Ende-zu-Ende-Pipeline funktioniert.** Ein echter, aus einer früheren ALVR-Session aufgezeichneter JPEG-XS-Frame (`C:\Temp\alvr_stream_jpegxs_svtjpegxs.jxs`, 4288×1664, YUV420, 8-bit) wird per WASM dekodiert und per WebGPU-Shader (BT.709 YUV→RGB) korrekt im Browser gerendert — Pixelfarbe stimmt exakt mit der von Hand berechneten Erwartung überein.

### Toolchain-Setup
- Emscripten SDK installiert nach `C:\Temp\emsdk` (via `git clone` + `emsdk.bat install/activate latest`, Version 6.0.8).
- **Blocker dabei**: `emsdk.bat`/`emcc.exe` rufen intern `python` auf; Windows' "App Execution Alias"-Stub für `python` (kein echtes Python installiert) täuscht einen Store-Installationsdialog vor statt "not found" zu melden. Fix: echtes Python 3.12 via `winget install -e --id Python.Python.3.12` installiert (keine Systemeinstellungen geändert, nur eine reguläre Programminstallation) und dessen Verzeichnis vor den Alias-Stub in PATH gesetzt.
- Node.js kommt mit emsdk mit (`C:\Temp\emsdk\node\<version>\node.exe`), separat für schnelle Tests ohne Browser-Overhead genutzt.

### Nötige Patches am SVT-JPEG-XS-Quellcode (`C:\Temp\SVT-JPEG-XS`, unversioniert, 2 kleine Änderungen)
Entgegen der ursprünglichen Befürchtung ("Blocker: hart x86_64-abhängig") kompilieren **alle 24 portablen C-Dateien** (`Common/Codec/*.c`, `Common/Codec/Threads/*.c`, `Decoder/Codec/*.c`) mit `emcc` ohne `ARCH_X86_64`-Define bis auf zwei Stellen:
1. `Source/Lib/Common/Codec/common_dsp_rtcd.c` + `.h`: `get_cpu_flags()` war nur unter `#ifdef ARCH_X86_64` deklariert/definiert, aber in `DecHandle.c`/`EncHandle.c` **unbedingt** aufgerufen — ein echter upstream-Portabilitätsbug (träfe genauso jeden nativen ARM-Build, z. B. Apple Silicon). Fix: `#else`-Zweig ergänzt, der `get_cpu_flags()` als `return 0;` bereitstellt; Deklaration in der Headerdatei von der `#ifdef` befreit.
2. Fehlende Include-Pfade: `decoder_dsp_rtcd.c` inkludiert unbedingt SIMD-Header (`Dwt53Decoder_AVX2.h` etc.) rein für Funktionsdeklarationen (die dank leerem `SET_FUNCTIONS_X86`-Makro im Nicht-x86-Fall nie aufgerufen werden) — einfach zusätzliche `-I`-Pfade auf `Common/ASM_SSE2`, `Common/ASM_AVX2`, `Decoder/ASM_SSE4_1`, `Decoder/ASM_AVX2`, `Decoder/ASM_AVX512` ergänzt, keine `.c`-Dateien daraus kompiliert.

Das Threading-Modell (`Source/Lib/Common/Codec/Threads/SvtThreads.c`) ist bereits reines POSIX (`pthread_create`, `sem_t`, `pthread_mutex/cond`) im Nicht-Windows-Zweig — lässt sich 1:1 mit Emscriptens `-pthread` (Web Workers + SharedArrayBuffer) verwenden, kein Refactoring nötig.

### Neue Dateien (Test-/Build-Setup, alle unversioniert)
- `C:\Temp\SVT-JPEG-XS-wasm\jxs_wasm_bridge.c` — dünner C-Wrapper um die offizielle Decoder-API (`svt_jpeg_xs_decoder_*`, Ablauf 1:1 aus `Source/App/SampleDecoder/main.c` übernommen), exportiert einfache Funktionen (`jxs_probe_frame_size`, `jxs_create`, `jxs_decode_frame`, `jxs_get_plane_ptr/size/width/height`, `jxs_destroy`).
- `C:\Temp\SVT-JPEG-XS-wasm\try_compile.sh` — kompiliert alle 24 Decoder-Quelldateien einzeln zu `obj/*.o` (aktuell `-O3 -msimd128 -pthread`).
- `C:\Temp\SVT-JPEG-XS-wasm\build_node.sh` / `build_web.sh` — linken die `.o`-Dateien + Bridge zu einem WASM-Modul für Node- bzw. Browser-Ziel (`MODULARIZE`, `EXPORT_NAME=JXSModule`, `-pthread -sPTHREAD_POOL_SIZE=8`, `-sINITIAL_MEMORY=256MB -sALLOW_MEMORY_GROWTH=1`).
- `C:\Temp\SVT-JPEG-XS-wasm\test_decode.js` — Node-Testskript, dekodiert einen Frame aus einem Sample-Chunk und dumpt Rohebenen zur Sichtprüfung.
- `C:\Temp\SVT-JPEG-XS-wasm\web\index.html` — Browser-Demo: lädt `sample_chunk.jxs` (20 MB Ausschnitt aus dem großen Rohstream), findet Frame-Grenzen selbst (`jxs_probe_frame_size`), dekodiert per WASM, lädt die drei YUV-Ebenen als `r8unorm`-WebGPU-Texturen hoch und rendert sie über einen Fullscreen-Triangle-Shader mit BT.709-YUV→RGB-Konvertierung (Koeffizienten 1:1 aus `JpegXsSideEncoder.cpp` übernommen: full-range BT.709, `R=Y+1.5748V`, `G=Y-0.1873U-0.4681V`, `B=Y+1.8556U`).
- `C:\Temp\SVT-JPEG-XS-wasm\web\serve.js` — Minimal-HTTP-Server, der `Cross-Origin-Opener-Policy: same-origin` + `Cross-Origin-Embedder-Policy: require-corp` setzt (Pflicht für `SharedArrayBuffer`/pthreads im Browser) sowie Range-Requests unterstützt (für spätere Chunked-Streams).

### Wichtiger Stolperstein bei der Verifikation
`requestAnimationFrame` feuert in unsichtbaren/Hintergrund-Tabs praktisch nicht (Browser-Drosselung) — im Test-Setup (Claude Code Browser-Pane wird nicht immer visuell dargestellt) blieb der Canvas deshalb zunächst schwarz, obwohl Dekodierung/Rendering-Code korrekt war. Fix für die Verifikation: Loop auf `setTimeout(tick, 0)` umgestellt (läuft unabhängig von Sichtbarkeit). **Für den Produktivfall auf Vision Pro (immer sichtbarer Tab) sollte wieder `requestAnimationFrame` verwendet werden** (vsync-gekoppelt, glatter) — das ist in Abschnitt 9 als offener Punkt vermerkt.

### Performance (Decode-only, ohne Render/Upload, gemessen in einem Hintergrund-Tab dieser Test-Umgebung — reale Zahlen auf sichtbarem Tab/echter Hardware potenziell besser)
- `-O2`, `pthread_pool=4`: **~18,7 fps**
- `-O3 -msimd128`, `pthread_pool=4`: **~58,5 fps** (reines Compiler-Flag, +3×)
- `-O3 -msimd128`, `pthread_pool=8`: **~161 fps**

Das deutet stark darauf hin, dass 90fps auf echter Hardware (Apple M2/M5 in der Vision Pro) im Rahmen des Möglichen liegt, sofern der reale Foreground-Tab nicht durch Browser-Timer-Drosselung ausgebremst wird (siehe Stolperstein oben). Die getestete Datei enthielt nur 3 (identische) Frames à 5.351.424 Bytes — für einen echten Bewegtbild-Test muss ein längerer/vielfältigerer Ausschnitt aus der Rohdatei gezogen werden.

### Update: echtes Bewegtbild bestätigt (2026-08-22, selber Tag)
Ursprünglicher Demo-Stand hatte nur die ersten 20 MB / 3 Frames der Datei geladen — alle identisch (SteamVR-Ladebildschirm), daher rein blau. Fix: `serve.js` bekam eine Route `/full_stream.jxs`, die direkt auf die volle 82-GB-Datei zeigt (per Range-Requests, nie komplett geladen); `index.html` scannt jetzt sequenziell per kleinen Header-only-Range-Requests (4 KB) die ersten 1800 Frames (deckt ~9,6 GB der Datei ab), merkt sich nur Offset+Größe pro Frame, und lädt beim Abspielen die tatsächlichen Framedaten erst on-demand nach (mit 1-Frame-Prefetch, Netzwerk und Decode überlappen). Vom Nutzer **in echtem Chrome (nicht nur in der Testumgebung) verifiziert**: nach den ersten Frames wechselt das Bild von der blauen Fläche zu echtem, deutlich komplexerem Spielinhalt.

### Update: Performance-Untersuchung 90fps-Ziel (2026-08-22, selber Tag)

Frage: Wie viele Threads laufen aktuell, kann mehr auf die GPU ausgelagert werden, wie kommen wir auf 90fps?

**GPU-Auslagerung — ehrlicher Stand**: Die GPU macht aktuell nur die finale YUV→RGB-Konvertierung + Textur-Rendering (WebGPU-Fragment-Shader). Das eigentliche JPEG-XS-Decoding (Entropie-Decode, Dequantisierung, inverse Wavelet-Transformation, Farbrücktransformation) läuft komplett auf der CPU über WASM — SVT-JPEG-XS hat **keinen** GPU-Compute-Pfad, das müsste man als eigene WebGPU-Compute-Shader von Grund auf neu schreiben (kein Flag zum Umschalten, ein echtes Entwicklungsprojekt für sich).

**Kritischer Bug gefunden und gefixt**: Der ursprüngliche Benchmark (58–161 fps aus der vorigen Session) hat unbemerkt nur den flachen blauen Ladebildschirm-Frame gemessen (fast nur Null-Koeffizienten, trivial zu dekodieren). Mit einem echten Spielinhalts-Frame lag die tatsächliche Decode-Geschwindigkeit nur bei **~32–35 fps**, deutlich unter dem 90fps-Ziel.

**Zweiter, gravierenderer Bug**: Der Decoder wurde bisher direkt vom Browser-Haupt-Thread aus aufgerufen (blockierende `svt_jpeg_xs_decoder_send_frame`/`get_frame`-API). Ein Benchmark-Sweep über mehrere Thread-Zahlen ließ die komplette Seite einfrieren (selbst `1+1` in der Konsole timete nach 30s aus) — Emscriptens Thread-Synchronisierung verträgt sich nicht gut mit blockierenden Aufrufen vom Haupt-Thread aus (kein echtes Sleep, sondern Spin-Wait-artiges Verhalten). Das erklärt vermutlich auch einen Teil der schlechten Skalierung bei mehr Threads.

**Fix (durchgeführt)**: Kompletter Umbau auf einen dedizierten Web Worker für das gesamte Decoding (`decode-worker-postjs.js`, per `--extern-post-js` in `jxs_decoder.js` eingebaut). Der Haupt-Thread ruft nie mehr direkt in den Decoder, sondern schickt nur noch Nachrichten an den Worker und bekommt fertig dekodierte Y/U/V-Buffer per Transferable zurück.

**Dabei gefundener echter Emscripten-Bug** (nicht projektspezifisch, generisches Gotcha): Wird ein Pthread-fähiges Emscripten-Modul per `importScripts()` in einen *anderen* Worker geladen, berechnet Emscripten die URL für seine eigenen Pthread-Pool-Worker aus `self.location.href` — das zeigt dann auf den *Wrapper-Worker*, nicht auf das eigentliche Modul-Skript, wodurch `JXSModule()` beim Aufbau des Thread-Pools für immer hängt. Fix: eigenen Code per `emcc --extern-post-js` direkt an `jxs_decoder.js` anhängen (`--post-js` reicht NICHT, landet innerhalb der Modul-Factory-Funktion statt danach) und den Worker direkt mit `new Worker('jxs_decoder.js')` starten, nicht über einen Wrapper.

**Offen/unklar**: In der hiesigen (gesandboxten) Test-Browser-Umgebung hängt `JXSModule()` im Worker weiterhin beim eigentlichen Spawnen der Pthread-Pool-Worker (selbst bei `PTHREAD_POOL_SIZE=1`), obwohl ein einfacher, nicht-Emscripten-nested-Worker-Test dort funktioniert. Unklar, ob das eine echte Browser-Einschränkung ist oder ein Artefakt der Test-Sandbox — **muss im echten Chrome des Nutzers verifiziert werden** (dort haben vorherige Tests ja funktioniert, nur eben mit dem Haupt-Thread-Blocking-Problem).

**Update — im echten Chrome verifiziert**: Der Worker-Umbau läuft dort sauber durch (Indexierung, Wiedergabe, sichtbares Bild). Das Hängen war ein Artefakt der Test-Sandbox.

**Update — Thread-Skalierung, mit echtem Frame-Inhalt gemessen** (nach Fix diverser Nebenbugs, siehe unten): 1 Thread ~8,5fps, 2 Threads ~8,0fps (Ausreißer), 4 ~20,3fps, 6 ~38,5fps, 8 ~51,8fps, 10 ~62,1fps — Kurve flacht bis 10 Threads noch nicht ab, PTHREAD_POOL_SIZE wurde auf 32 erhöht um Kopf-an-Kopf-Bedarf von Live-Wiedergabe + Bench-Tests abzudecken.

**Update — Profiling-Fund**: Ein Aufschlüsseln nach Dequantisierung/Entropie-Unpacking/inverser-Wavelet-Transformation (eigene Instrumentierung in `jxs_wasm_bridge.c`, wrapt die RTCD-Funktionszeiger) zeigt: Dequant (~3,1ms/Thread) und Unpack (~3,5ms/Thread) sind die größten Posten, IDWT (~1,4ms/Thread) überraschend klein — obwohl IDWT der einzige Kandidat mit vorhandenem, aber nicht nutzbarem AVX2-Kern ist. Für Dequant existiert ein SSE4.1-Kern (`Dequant_SSE4.c`), der über Emscriptens x86-Intrinsics-auf-WASM-SIMD128-Shim direkt kompilierbar wäre (SSE4.1 ist 128-bit, AVX2 nicht) — vielversprechendster nächster SIMD-Kandidat.

**Update — "alter Kontext ist ~40% langsamer als neuer"-Rätsel: gelöst, war ein Mess-Artefakt, kein echtes Problem.** Ursprünglich vermutet: Betriebssystem-Thread-Scheduling (Hybrid-CPU P-/E-Kerne). Tatsächliche Ursache: **alle "schnellen" Referenzmessungen (Bench-Sweep, Profile, Recreate) dekodierten denselben einzelnen Frame immer wieder** — das lässt CPU-Cache/Sprungvorhersage sich auf genau dieses Bitmuster spezialisieren und macht die Messung künstlich schnell (~16-19ms bei 8-10 Threads). Ein neuer Sweep mit *unterschiedlichen* Frames pro Thread-Zahl (`window.__jxsBenchVaryingSweep`/"Realistic sweep"-Button) zeigt die ehrlichen Zahlen: **4 Threads ~63ms (15,8fps), 6 ~32ms (31,2fps), 8 ~24ms (41,4fps), 10 ~20ms (50,3fps)** — spürbar langsamer als die same-frame-Zahlen, aber sehr nah an dem, was die echte Live-Wiedergabe tatsächlich zeigt (~22-26ms Decode bei 8 Threads, mit wechselnden Frames) → **die Live-Wiedergabe war die ganze Zeit korrekt, nur der Vergleichsmaßstab war falsch.** Die periodische Kontext-Neuerstellung (`maybeRecreateContext`) behebt entsprechend nichts Reales, ist aber harmlos und kann drin bleiben oder wieder raus.

Nebenbei behoben: Ein Instrumentierungs-Bug, bei dem *jede* neue Decoder-Initialisierung (nicht nur die erste) intern die RTCD-Funktionszeiger unconditionally auf die unverpackten C-Versionen zurücksetzt, wodurch das Profiling-Tooling nach der zweiten Kontext-Erstellung nur noch Nullen zeigte (betraf nur das Profiling selbst, nicht die eigentliche Dekodierung).

**Aktueller ehrlicher Stand**: Live-Kontext läuft jetzt mit 10 statt 8 Threads (realistischer Sweep zeigte bei 10 noch spürbaren Zugewinn: 50,3 vs. 41,4fps bei 8). Fetch-Overhead (~15-18ms/Frame) ist ein Artefakt des dateibasierten Test-Setups (Range-Requests gegen die 82-GB-Datei über den simplen Node-Dev-Server) und entfällt im echten Live-Stream (WebSocket/WebTransport, Daten schon im Speicher, kein Datei-Seek). Reine Decode-Zeit bei 10 Threads (~20ms, 50fps) ist der ehrliche Zielwert für die 90fps-Frage — noch ca. Faktor 2 vom Ziel entfernt.

## 12. GPU-Offload-Machbarkeitsstudie (2026-08-22, selber Tag)

Frage: Lässt sich Dequant/IDWT/Farbtransformation auf die GPU auslagern (WebGPU-Compute), statt nur SIMD auf der CPU nachzurüsten? Antwort: **Ja, mathematisch und für die Korrektheit eindeutig bestätigt** — alle drei getesteten Bausteine liefen bit-exakt korrekt auf der GPU:

| Baustein | GPU-Dispatch (eingeschwungen, ohne Rücklese-Overhead) | Korrektheit |
|---|---|---|
| Dequantisierung (`dequant_c`, komplett) | **1,12 ms** für 10,7 Mio. Koeffizienten | PERFECT MATCH (0/10.702.848) |
| Horizontale IDWT (`idwt_horizontal_line_lf16_hf16_c`) | **3,44 ms** für 9,17 Mio. Ausgabewerte | PERFECT MATCH (0/9.171.496) |
| Vertikale IDWT, Normalfall (`idwt_vertical_line_c`, nur der häufigste der 5 Zweige — nicht Precinct-Rand) | **1,64 ms** für 12,4 Mio. Ausgabewerte | PERFECT MATCH (0/12.443.776) |

Methodik: Die jeweilige CPU-Funktion wird über den bestehenden RTCD-Funktionszeiger-Mechanismus umhüllt (`jxs_wasm_bridge.c`), zeichnet während einer echten Dekodierung alle Aufrufe (Eingabedaten + das korrekte CPU-Ergebnis) in fest vorallozierte Puffer auf, die dann unverändert als WebGPU-Compute-Shader (direkte 1:1-Übersetzung der C-Logik, keine Neuherleitung) nachgespielt und Wert für Wert verglichen werden — ohne die echte Pipeline anzufassen. Buttons dafür in `web/index.html`: "GPU dequant/IDWT (horizontal/vertical) feasibility test".

**Wichtiger Bug unterwegs (behoben)**: Die Aufzeichnungspuffer für die vertikale IDWT waren mit 6 Mio. Elementen zu knapp bemessen (ein echter Frame brauchte 6,22 Mio.) — die Zähler-Funktionen gaben aber unclamped die rohe (zu große) Reservierungszahl zurück, was in JS zu einem Lesezugriff über die tatsächliche Pufferallokation hinaus führte (kein Crash, aber eine riesige ungewollte Speicherkopie, die den ganzen Rechner kurz spürbar ausgebremst hat). Fix: Puffer vergrößert (10 Mio.) **und** alle Zähler-Getter (auch bei Dequant/horizontaler IDWT, wo es zufällig knapp gutging) hart auf die tatsächliche Puffergröße geclampt — Lehre: bei jedem neuen Aufzeichnungspuffer von Anfang an clampen, nicht erst wenn's passiert.

**Warum trotzdem (noch) keine echte Integration**: Die reale Decoder-Pipeline ist **Slice-basiert**, nicht precinct-basiert — ein Thread arbeitet ein komplettes Precinct von Unpack bis IDWT/Farbe am Stück ab, mit `sync_slices_idwt`/`map_slices_decode_done`-Condition-Variablen zur Cross-Slice-Synchronisation für die vertikale IDWT (die Nachbarzeilen aus *anderen* Slices/Threads braucht). Eine einzelne GPU-Sammelstelle pro Frame ("erst alle unpacken, dann GPU, dann weiter") bräuchte eine **neue Wartestelle in dieser bereits fein hand-synchronisierten Multi-Thread-Pipeline** — echtes Risiko für subtile, schwer zu findende Bugs (falsche Precinct-Reihenfolge, kaputte Cross-Slice-Sync). Bewusst als eigenes, separates Vorhaben für eine dedizierte Sitzung zurückgestellt, nicht an einem Nachmittag nebenbei gemacht.

Auch offen (nicht mehr getestet): die anderen 4 der 5 Zweige von `idwt_vertical_line_c` (Precinct-Rand-Fälle: `height==2`, `first_precinct`, `last_precinct` gerade/ungerade) und die Farbrücktransformation (MCT) — vermutlich ähnlich einfach wie Dequant, aber nicht verifiziert.

### Nächste konkrete Schritte für die Fortsetzung
1. **Echte Pipeline-Integration** (eigene Sitzung): Slice-Pipeline um eine Zwei-Phasen-Barriere erweitern (CPU: nur Unpack für den ganzen Frame, dann GPU: Dequant+IDWT+Farbe, dann CPU macht ggf. Rest weiter) — größtes Risiko-Item, siehe oben.
2. Restliche vertikale IDWT-Zweige (Precinct-Rand) und Farbtransformation (MCT) genauso validieren, um das volle Bild zu haben, bevor die Integration beginnt.
3. `requestAnimationFrame` statt `setTimeout` im produktiven Pfad, echten Vsync-Takt messen.
4. `MAX_INDEX_FRAMES` (aktuell 1800) ggf. erhöhen bzw. Indexierung fortlaufend statt einmalig begrenzt gestalten, für beliebig lange Wiedergabe über die ganze 82-GB-Datei.
5. Live-Quelle statt statischer Datei (WebSocket/WebTransport) anbinden — behebt nebenbei den Fetch-Overhead komplett.
6. Auf echtem Vision-Pro-Gerät testen (Safari-Verhalten bzgl. COOP/COEP/SharedArrayBuffer kann von Desktop-Chromium abweichen).
7. WebXR-Stereo-Ausgabe ergänzen (siehe Abschnitt 8).
8. Alternativ/ergänzend zur GPU-Integration: SIMD-Portierung (Dequant via Emscripten-SSE4.1-Shim direkt kompilierbar) — kleinerer, weniger riskanter Hebel, falls die GPU-Pipeline-Integration zu lange dauert.

## 14. WASM-SIMD-Dequant — validiert und live geschaltet (2026-08-23)

Anlass: `-msimd128` steht seit Anfang an im Build, aber Disassemblieren des kompilierten `.wasm` (per `wasm-dis`, Funktionen einzeln nach SIMD-Opcodes durchsucht) zeigte, dass `dequant_c` **0 von 206 Zeilen** automatisch vektorisiert bekam (zu verzweigt für LLVMs Auto-Vektorisierer), während IDWT/Unpack immerhin 3-7% abbekamen. `dequant_c` macht laut Profiling ~30% der Decode-Zeit aus — der größte ungenutzte Hebel.

Fix: `dequant_wasmsimd()` in `jxs_wasm_bridge.c`, 1:1 nach der bestehenden x86-SSE4.1-Referenz (`Decoder/ASM_SSE4_1/Dequant_SSE4.c`) auf `wasm_simd128.h`-Intrinsics portiert statt neu hergeleitet (die APIs sind sich sehr ähnlich, z. B. `_mm_add_epi16` ↔ `wasm_i16x8_add`). Validiert über dieselbe Capture-Infrastruktur wie die GPU-Tests: echte Decode-Aufrufe aufgezeichnet, SIMD-Version auf einer Kopie der Eingaben abgespielt, gegen die vom echten `dequant_c` erzeugte Referenz verglichen — **PERFECT MATCH, 0 von 10.702.848 Koeffizienten weichen ab**, SIMD-Laufzeit ~3 ms gegenüber geschätzt ~23-31 ms für dieselbe Arbeit seriell.

Live-Wiedergabe läuft jetzt über `dequant_wasmsimd()` (`install_instrumentation()` in `jxs_wasm_bridge.c` setzt `orig_dequant = dequant_wasmsimd` statt `dequant_c`). Wichtig: Die Referenz-Aufzeichnung für künftige Validierungs-Läufe (`g_cap_coeff_expected`) ruft weiterhin explizit die echte `dequant_c` auf einer Kopie auf, statt einfach das Ergebnis von `orig_dequant` zu übernehmen — sonst würde der Validierungs-Button nur die SIMD-Version gegen sich selbst vergleichen und jeder Lauf würde trivial "matchen".

Nebenbei gefunden und gefixt: eine vorbestehende Race Condition im Pause/Resume-Mechanismus (`requestNextFrame()` in `index.html`) — beim Pausieren für einen Test-Button konnten zwei überlappende `requestFrame`-Anfragen an den Worker gehen, die dieselbe bereits aufgelöste Decode-Promise doppelt abwarteten und denselben (schon transferierten, also "detached") Frame-Puffer ein zweites Mal zu senden versuchten (`DataCloneError`). Harmlos (Wiedergabe erholte sich von selbst), aber jetzt per In-Flight-Flag (`frameRequestInFlight`) unterbunden.

**Update selber Tag**: `idwt_horizontal_line_lf16_hf16_wasmsimd()` nach demselben Muster ergänzt — 1:1 von der bestehenden x86-AVX2-Referenz (`Decoder/ASM_AVX2/Dwt53Decoder_AVX2.c`) auf 128-bit WASM-SIMD halbiert (4 statt 8 Elemente pro Batch). Die "even"-Ausgaben hängen nur von lf/hf ab (parallelisierbar), "odd" braucht den vorherigen "even"-Wert — über die Batch-Grenze hinweg gelöst durch einen mitgeführten Skalar (`prev_even`) + `wasm_i32x4_shuffle` (nimmt zwei Quell-Vektoren direkt entgegen, einfacher als der AVX2-Cross-Lane-Permute-Trick). Validiert über dieselbe Capture2-Infrastruktur wie der GPU-Test — **PERFECT MATCH, 0 von 9.171.496 Werten weichen ab**, dreimal wiederholt. Live-Wiedergabe läuft jetzt auch hierüber.

Ausgeschlossen für SIMD (zumindest vorerst): `unpack_data_c` — es gibt dort zwar auch eine AVX2-Referenz, aber sie beschleunigt nur die Bit-Verteilung *innerhalb* einer Gruppe (4 Lanes) sowie eine unabhängige Vorab-Summierung, **nicht** das eigentliche sequentielle Bit-Lesen aus dem gemeinsamen Bitstream-Zustand (das bleibt zwingend seriell, ein Fehler dort zerstört die gesamte restliche Bitstream-Position, nicht nur einen Pixel). Erfordert außerdem komplett neue Capture-Infrastruktur (gibt es für Unpack noch nicht). Größerer Aufwand, höheres Korrektheitsrisiko, geringerer erwarteter Gewinn als bei Dequant/IDWT — zurückgestellt.

Nächster Kandidat für dieselbe Behandlung: `idwt_vertical_line_c` (default-Zweig, schon per Capture3 instrumentiert und GPU-validiert).

## 15. GPU-Pipeline-Integration — Scoping-Versuch, zurückgestellt (2026-08-23)

**Git-Sicherung eingerichtet**: `SVT-JPEG-XS-wasm` ist jetzt ein eigenes Git-Repo (`git init`, `.gitignore` für obj/, Testdateien, private Keys). Tag `baseline-cpu-simd-working` markiert genau den validierten Stand nach Abschnitt 14 (Dequant + horizontale IDWT als SIMD, live, ~48 FPS Wiedergabe / ~68 FPS Decode-Obergrenze auf der Vision Pro). Rollback jederzeit über `git checkout baseline-cpu-simd-working`.

Auf Wunsch versucht: die in Abschnitt 12 beschriebene "CPU entpackt, GPU rechnet Dequant+IDWT für den ganzen Frame" Idee tatsächlich umzusetzen. Ergebnis: **beim genauen Hinsehen deutlich größerer Umbau als angenommen, zurückgestellt.**

**Technischer Fund 1 — Sync-C-ruft-Async-GPU-Problem**: WebGPU ist nur aus JS aufrufbar, nie aus C/WASM. Die tragfähige Lösung wäre, den Decode-Aufruf in zwei separate, gewöhnliche (synchrone) WASM-Aufrufe zu splitten — Phase 1 "nur entpacken", dazwischen macht JS in aller Ruhe den asynchronen GPU-Dispatch, Phase 2 "fertigstellen mit GPU-Ergebnissen". Das umgeht Atomics/Asyncify/JSPI komplett, da zwischen den beiden Aufrufen kein C-Thread blockiert auf irgendetwas wartet.

**Technischer Fund 2 — der eigentliche Blocker**: Die IDWT-Rekonstruktion in `DwtDecoder.c` (`transform_component_line_V1_Hx`/`V2_Hx` + ihre `_recalc`-Varianten) ist eine bewusst speichersparende *Streaming*-Wavelet-Transformation mit einem kleinen gleitenden Fenster, nicht mit Puffern für den ganzen Frame. Belegte Puffergröße laut Code (`Decoder.c`, `precinct_idwt_tmp_buffer`): nur **1× bis 7,5× die Bildbreite** (`(7 * V1_len + 4 * pi->width) * sizeof(int32_t)` für den größten Fall), wiederverwendet über hunderte Precincts pro Slice hinweg. "Erst für den ganzen Frame entpacken, dann in einem Rutsch GPU-rechnen" würde bedeuten, dieses Streaming-Design durch frame-weite Puffer zu ersetzen — keine Wrapper-Änderung, sondern eine Neuimplementierung des Kern-Rekonstruktionsalgorithmus.

**Technischer Fund 3 — beide Pfade werden gebraucht**: Per Diagnose-Build bestätigt (Node-Testharness `test_decode.js` gegen `sample_chunk.jxs`, echte ALVR-Aufnahme): Luma läuft mit `decom_v=2` (der komplexere 4-Stufen-Pfad `transform_component_line_V2_Hx`), Chroma (Cb/Cr, wegen 4:2:0-Subsampling) mit `decom_v=1` (der einfachere 2-Stufen-Pfad `transform_component_line_V1_Hx`). Beide Pfade sind für jeden einzelnen Frame relevant — keine Abkürzung über "nur ein Pfad kommt vor".

**Nebenbei interessant**: `dequant()` selbst arbeitet bereits auf einem größeren, precinct-übergreifenden Koeffizientenpuffer (`ctx->coeff_buff_ptr_16bit`, adressiert über `precinct_line_idx`), nicht auf einem kleinen Ringpuffer — das erklärt rückblickend, warum der Dequant-SIMD-Umbau in Abschnitt 14 so unkompliziert war. Das Ringpuffer-Problem betrifft ausschließlich die IDWT-Stufe.

**Entscheidung**: Auf Nutzerwunsch hier gestoppt, CPU-SIMD-Stand (Abschnitt 14) bleibt der aktuelle Endpunkt dieses Prototyps. Bei Fortsetzung in einer dedizierten Sitzung: mit dem einfacheren V1_Hx-Pfad (Chroma) anfangen, um den Umbau-Mechanismus (Ringpuffer → frame-weite Puffer, aufgeschobene Berechnung, Zwei-Phasen-WASM-Aufruf-Split) isoliert bit-exakt zu validieren (Node-Testharness eignet sich dafür gut — Plane-Dumps lassen sich byte-für-byte vergleichen), bevor der komplexere V2_Hx-Pfad (Luma) angegangen wird. Realistische Einschätzung: mehrere Tage Arbeit, nicht mehr an einem Nachmittag zu schaffen.

## 16. ALVR-Foveated-Encoding-Entzerrung im WebXR-Test (2026-08-23)

Erkannt: Der WebXR-Test zeigte den Stream bisher unverzerrt-linear, obwohl ALVR beim Aufnehmen "Foveated Encoding" aktiv hatte (staucht den Rand-Bereich vor dem Encodieren, um dort Bits zu sparen — mehr übertragene Pixel für die Bildmitte, weniger für die Peripherie). Ohne Rückwärts-Entzerrung beim Anzeigen wirkt das ganze Bild leicht "eingedrückt".

**Umsetzung**: Die exakte Inverse-Warp-Formel aus ALVRs eigenem Client-Shader (`alvr/graphics/resources/stream.wgsl`, `fragment_main`) 1:1 nach GLSL portiert (nicht neu hergeleitet) — Mitte wird linear abgetastet, die beiden Rand-Bereiche über die quadratische Auflösung der jeweils inversen linear-geblendeten Kompression (siehe `alvr/graphics/src/stream.rs::foveated_encoding_shader_constants` für die Koeffizienten-Herleitung). Die Koeffizienten werden einmalig in JS berechnet (`computeFoveationCoeffs()` in `web/index.html`) und als Uniforms gesetzt.

**Eingabewerte** — aus der echten `session.json` gelesen (nicht die Doku-Defaults angenommen, auch wenn es hier zufällig dieselben Werte sind, da nie angepasst): `foveated_encoding` mit `center_size_x/y=0.45/0.4`, `center_shift_x/y=0.4/0.1`, `edge_ratio_x/y=4.0/5.0`, `enabled: true`. Auflösung: `emulated_headset_view_resolution` = 3660×3200 pro Auge — aber das ist nicht direkt die richtige Eingabe für die Formel! Tatsächlich gebraucht: 3648×3200 (3660 abgerundet auf ein Vielfaches von 32, da GPU-Render-Targets aligned alloziert werden). **Bestätigt durch Rücktest**: Mit 3648×3200 als Eingabe reproduziert die Formel exakt unsere beobachtete Decodier-Auflösung (2144×1664 pro Auge) auf beiden Achsen — das war der Beweis, dass die Eingabewerte stimmen, nicht nur plausibel aussehen (mit dem rohen 3660 traf nur die Höhe exakt, die Breite lag einen Alignment-Schritt daneben).

**Sichtbarer Effekt**: Der äußere Bildbereich wird beim Sampling sichtbar gestreckt (unschärfer als vorher) — das ist kein Fehler, sondern genau die Auflösungs-Ersparnis von Foveated Encoding, jetzt korrekt statt komprimiert-falsch dargestellt.

**Nicht validierbar wie der Codec-Code**: Anders als bei Dequant/IDWT gibt es hier keinen bit-exakten Referenzwert zum Gegenprüfen — nur visuelle Beurteilung (Rand wirkt nicht mehr gestaucht) oder Vergleich mit echtem ALVR-Client-Output.

## 13. Echter Gerätetest auf der Vision Pro (2026-08-23)

Ziel war ein direkter Cross-Device-Vergleich (PC vs. Vision Pro) mit denselben Diagnose-Tools, per neuem Remote-Log-Upload (`/upload-log`-Route in `serve.js`, Button "Send log to server" in `index.html`) statt manuellem Copy-Paste.

**Unterwegs gefundene und behobene Bugs** (alle in `decode-worker-postjs.js` / `web/index.html` / `serve.js`):
- `ensureModuleLoaded()` cachte nur das *fertige* Modul, nicht das laufende Promise — bei zwei sich überlappenden Aufrufen (z. B. hängender erster Versuch + Retry-Klick) konnte `JXSModule()` doppelt gestartet werden. Fix: gemeinsames In-Flight-Promise.
- Reines `https://` (nötig fürs LAN, self-signed Zertifikat für die Vision Pro) blockierte auf dem PC das Auswählen der vollen 82-GB-Datei — `JXSModule()` hing beim Laden fest, sobald diese große Datei am Worker hing. Ursache nicht abschließend isoliert, aber reproduzierbar nur unter HTTPS+self-signed. Fix: Server hört jetzt zusätzlich auf reinem `http://localhost:8780` (nur PC, kein Zertifikat nötig), `https://<LAN-IP>:8781` bleibt für die Vision Pro.
- Der lokale-Datei-Pfad (`useLocalFile`) setzte nie `liveThreadCount` — lief always mit dem Default von 8 Threads, unabhängig davon, was der Thread-Sweep als optimal ermittelt hatte. Fix: Thread-Count wird jetzt genauso wie beim Netzwerk-Pfad übergeben und angewendet.
- Mit höherer Live-Thread-Zahl reichte `PTHREAD_POOL_SIZE=32` nicht mehr: Die laufende Wiedergabe hält dauerhaft `liveThreadCount` Worker-Threads belegt, periodische Context-Neuerstellung verdoppelt das kurzzeitig, und ein gleichzeitig laufender Sweep-Test braucht eigene Threads obendrauf — bei genug gleichzeitiger Last blockiert das Anlegen neuer Pthreads für immer (echter Deadlock, kein JS-Fehler, da der Worker-Thread nicht abstürzt, sondern nur nie mehr antwortet). Fix: Pool auf 128 erhöht.

**Zentraler Befund — Vision Pro hat ein CPU-Plateau, nicht bloß ungenutzte Kerne:**

| Threads | PC (Decode-only) | Vision Pro (Decode-only) |
|---|---|---|
| 8 | 47,9 fps | 50,0 fps |
| 10 | 58,8 fps | 55,8 fps |
| 16 | 76,5 fps | 55,1 fps |
| 20 | 82,8 fps | **55,9 fps (Bestwert)** |
| 24 | 88,5 fps | 53,9 fps |
| 28 | 89,8 fps | 53,2 fps |
| 32 | **90,6 fps** | 54,9 fps |

Der PC skaliert bis 32 Threads praktisch durchgehend (mehr physische Kerne), die Vision Pro sättigt bereits bei ~10-20 Threads und wird darüber hinaus durch Oversubscription-Overhead sogar wieder minimal langsamer. Live-Wiedergabe (Fetch+Decode kombiniert) lag auf der Vision Pro entsprechend nur bei ~39-41 fps.

**Konsequenz**: Mit reinem CPU-Thread-Tuning ist auf der Vision-Pro-Hardware bei ~55-56 fps Decode-Kapazität Schluss — 90 fps sind über diesen Weg allein nicht erreichbar. Das macht Abschnitt 12 (GPU-Offload-Machbarkeitsstudie, bereits bit-exakt validiert für Dequant + horizontale/vertikale IDWT) zum wahrscheinlich einzigen verbleibenden Hebel für den Rest der Lücke — die dort als riskant zurückgestellte echte Pipeline-Integration wird damit wichtiger, nicht optional.

`THREAD_COUNT` in `index.html` ist jetzt auf 20 gesetzt (Vision-Pro-Bestwert), da das die Zielhardware ist — nicht mehr auf einen PC-optimierten Wert.

Zusätzlich diskutiert (noch nicht umgesetzt): `indexFrames()` — das schrittweise Scannen der Datei, um variable JPEG-XS-Framegrößen für wahlfreien Zugriff/Loop zu kennen — ist reine Testharness-Infrastruktur für die statische Datei und würde bei einem echten Frame-für-Frame-Live-Stream komplett entfallen. `MAX_INDEX_FRAMES` (aktuell 1800, ~9,6 GB) könnte für schnellere Testiterationen deutlich gesenkt werden (z. B. auf 200-300 Frames).

## 11. Nützliche Referenzen für die Fortsetzung

- ALVR-Repo: `C:\Temp\ALVR-org\ALVR-v20`
- SVT-JPEG-XS-Quellcode: `C:\Temp\SVT-JPEG-XS`
- Build: `cargo xtask build-streamer --release --keep-config` (im ALVR-Repo-Root ausführen)
- JPEG-XS-Dumps landen (Default) in `C:\Temp\alvr_stream_jpegxs_svtjpegxs.jxs` + `..._stats.csv`
- HEVC-Referenz-Dump: `C:\Temp\alvr_stream_<codec>_<encoder>.<ext>` + `..._stats.csv` (per StreamDumper)
