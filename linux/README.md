# Lipflow for GNOME

This frontend targets openSUSE Tumbleweed, GNOME 50, Wayland, and x86_64. All new application
logic is Rust. PyO3 embeds Python 3.12 to reuse the existing MediaPipe, PyTorch, Auto-AVSR,
cleanup, vocabulary, practice, history, and training implementations without changing them.

## Flatpak

GitHub Actions builds the CUDA-enabled x86_64 bundle on relevant pushes and pull requests.
You can also start it from **Actions → Linux Flatpak → Run workflow**. After a successful run,
download the **Lipflow-linux-x86_64-cuda** artifact and extract its ZIP. Artifacts are retained
for seven days. From the extracted directory:

```sh
sha256sum -c Lipflow.flatpak.sha256
flatpak install --user Lipflow.flatpak
flatpak run io.github.develop7.Lipflow
```

The [workflow](../.github/workflows/linux-flatpak.yml) builds from the pinned manifest,
installs the exported bundle, checks the embedded Python engine with `doctor`, and opens
the GTK window under a virtual display. NVIDIA acceleration and desktop portal consent
still need testing on a real GNOME session. CI does not download speech models.

To install the generated portable bundle, use:

```sh
flatpak install --user linux/flatpak/Lipflow.flatpak
flatpak run io.github.develop7.Lipflow
```

The bundle points to Flathub for the GNOME runtime. The adjacent
`Lipflow.flatpak.sha256` records its checksum.

The verified x86_64 bundle is about 2.8 GiB. The installed application uses about 6.9 GB,
plus the shared GNOME/NVIDIA runtimes and downloaded speech models.

Install Flatpak, flatpak-builder, and elfutils with your distribution's package manager.
Flatpak Builder uses elfutils (`eu-strip` and `eu-elfcompress`) to process debug symbols.
Build from the repository root:

```sh
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak-builder --user --install-deps-from=flathub --force-clean --install \
  linux/flatpak/build linux/flatpak/io.github.develop7.Lipflow.json
flatpak run io.github.develop7.Lipflow
```

The manifest selects GNOME Platform/SDK 50 and the Rust SDK extension. It bundles Python
3.12.14, PortAudio, the unchanged ML engine, and pinned Python 3.12 Linux wheels, including
PyTorch's CUDA 13 dependencies and safetensors for whisper mode. Downloads are checked against
SHA-256 hashes, and compilation and wheel installation run offline. Allow several GB for the
installed application and considerably more for the SDK, dependency cache, and build output.

To produce a portable bundle after building:

```sh
flatpak-builder --user --repo=linux/flatpak/repo --force-clean \
  linux/flatpak/build linux/flatpak/io.github.develop7.Lipflow.json
flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
  linux/flatpak/repo linux/flatpak/Lipflow.flatpak io.github.develop7.Lipflow
flatpak install --user linux/flatpak/Lipflow.flatpak
```

No speech model weights are redistributed in the package: the LRS3 weights are restricted to
non-commercial research use. **Dictate → Download models and load** fetches the original
~1.2 GB lip-reading models. Enabling whisper mode downloads another ~1.9 GB. Model downloads
use HTTPS, validate lengths and JSON, and publish completed files atomically.

### NVIDIA RTX 3080

The shared engine selects CUDA when available and otherwise uses CPU. The RTX 3080's Ampere
architecture is supported by the bundled CUDA build. Keep your Flatpak runtimes updated so
Flatpak can install the NVIDIA GL extension matching the host driver (595.104 in the requested
configuration). You do not need to install CUDA inside the host OS for the packaged wheels.

Flatpak shares the NVIDIA driver through its `org.freedesktop.Platform.GL.nvidia` extension.
Flathub's current runtime catalogue has no shared CUDA 13 runtime for this PyTorch build, so
the application's pinned CUDA, cuBLAS, and cuDNN libraries are bundled separately from the
shared driver.

```sh
flatpak update
flatpak --gl-drivers
flatpak run io.github.develop7.Lipflow doctor
```

`doctor` reports PyTorch, CUDA availability, the selected GPU, model presence, and portal
versions. GPU acceleration still needs verification with your host driver and camera. The
manifest grants `--device=dri`, which Flatpak also uses for the NVIDIA compute devices. Camera
access comes from a portal-provided PipeWire descriptor, without host video-device access.

## Desktop behavior

1. Open **Preferences → Connect desktop permissions**. GNOME asks for background/autostart,
   camera, global shortcut, and keyboard/clipboard permissions. Automatic paste requests only
   the keyboard device, without screen capture or pointer control.
2. Load the models. Hold **Ctrl+Alt+Space** while mouthing your sentence. Release to finish;
   recording includes the same 0.4-second tail as the shared implementation. Double-tap for
   hands-free, tap again to finish, or press **Ctrl+Alt+Esc** to cancel. Clips stop at 60 seconds.
3. The native window hides during automatic-paste dictation when background permission is
   granted. Put the cursor in the receiving app. Paste uses the Clipboard and RemoteDesktop
   portals, including transfer acknowledgement. A result is saved in history before pasting;
   permission or paste failures leave it available for copying.
   If Lipflow itself has focus when decoding finishes, the result is copied instead of pasted
   into its own window. Copy-only background operation uses the same portal clipboard session
   and consent, without injecting keyboard events.
4. **Change shortcuts…** opens GNOME's portal configuration. Permission revocation or a portal
   restart invalidates sessions and prevents further keyboard injection. Reconnect permissions
   to resume.
5. Closing the window keeps Lipflow ready when background permission was granted. Launch the
   app again or open a notification to show it. **Quit** exits it. **Start when I log in** uses
   the Background portal's autostart request.

Microphone capture is enabled only for whisper dictation. Silence or insufficient audio uses
the existing lip-only fallback. The camera opens on demand and releases after 45 idle seconds.
Practice shows 24 sentences, stores their known text with mouth crops, and runs the existing
face/phrase training with six held-out clips. Training pauses dictation and reports the score.

**Import my phrases…** uses the desktop file chooser portal. **Custom words** accepts one term
per line; imports also suggest names. History displays the most recent 100 entries and supports
copying. The Linux frontend does not read text fields or titles in other applications, so the
macOS automatic correction watcher and context-name extraction are not part of this port.

Cleanup options are automatic, offline formatting, local Ollama, and Claude. Automatic cleanup
uses the shared engine's selection order: Claude when a credential is present, then Ollama
when available, then offline formatting. MLX is Apple-only. Claude sends candidate text,
recent dictation context, and relevant personal phrases to Anthropic. Offline formatting and
local Ollama keep cleanup local. Configure optional API credentials through your environment;
they are not stored in the application's settings file.

### Stored data

Native installs use `$XDG_DATA_HOME/lipflow` (default `~/.local/share/lipflow`). Flatpak uses
`~/.var/app/io.github.develop7.Lipflow/data/lipflow`. This includes `linux-settings.json`,
`words.txt`, `phrases.txt`, `history.jsonl`, `Lipflow.log`, mouth clips, and personal model
weights. The recent-dictation clip preference controls shared clip retention. Practice clips
are retained explicitly for training; microphone audio is not persisted.

Models use `$XDG_CACHE_HOME/lipflow/models` (Flatpak:
`~/.var/app/io.github.develop7.Lipflow/cache/lipflow/models`). Development reuses an existing
checkout's `models/` folder. `LIPFLOW_HOME`, `LIPFLOW_MODELS`, `LIPFLOW_ENGINE_ROOT`, and
`LIPFLOW_PYTHON_SITE_PACKAGES` can override these paths.

## Native development

Install Rust 1.85 or newer, Python 3.12 with a shared library, GCC, pkg-config, GTK4 >= 4.12,
libadwaita >= 1.5, GStreamer core/app/video development packages, the GStreamer PipeWire plugin,
and PortAudio. On openSUSE, GTK and libadwaita development packages are `gtk4-devel` and
`libadwaita-devel`; install the corresponding GStreamer development and PipeWire plugin
packages provided by your current Tumbleweed snapshot. Use `uv` for the existing Python lock.

```sh
uv sync --frozen --python 3.12
uv pip install --no-deps --require-hashes -r linux/ml-extra-requirements.txt
cd linux
export PYO3_PYTHON="$(pwd)/../.venv/bin/python"
cargo build --locked
target/debug/lipflow doctor
target/debug/lipflow download-models --samples
target/debug/lipflow run
```

If your Python distribution reports a library directory that differs from its installed
shared library, set `LIBRARY_PATH` for the linker and `LD_LIBRARY_PATH` for execution to that
library directory. Run Cargo from `linux/`, where the Rust-version-compatible resolver
configuration is located. Use the Rust binary for Linux; the original Python desktop entry
point remains the macOS/Windows frontend.

Other Rust CLI commands:

```sh
target/debug/lipflow file ../samples/2016-03-12.mov --start 20.4 --end 28.1 --cleanup basic
target/debug/lipflow import-text my-phrases.txt
target/debug/lipflow run --background
target/debug/lipflow download-models --whisper
```

## Validation

```sh
cd linux
cargo fmt --check
cargo check --locked
cargo test --locked -- --test-threads=1
dbus-run-session -- xvfb-run -a target/debug/lipflow run --smoke-test
flatpak-builder --show-manifest flatpak/io.github.develop7.Lipflow.json
desktop-file-validate flatpak/io.github.develop7.Lipflow.desktop
appstreamcli validate --no-net flatpak/io.github.develop7.Lipflow.metainfo.xml
```

Rust tests exercise hold/double-tap/cancel timing, padded camera frames, settings migration,
wheel architecture selection, calls into the real Python preprocessing and cleanup modules,
and a private D-Bus portal fixture. The fixture exercises responses before method replies,
permission denial, shortcut activation/deactivation, keyboard-only device selection, consent
restoration tokens, real Unix-FD UTF-8 clipboard transfers, copy-only behavior, and revocation.
These tests do not substitute for GNOME's actual portal implementation or a real webcam/GPU.

Cloud verification completed the GNOME 50 Flatpak build and portable-bundle installation,
its GTK window startup on GNOME Platform 50, bundled CUDA-library imports with CPU fallback,
and real-video transcription through the installed Rust CLI. The existing Python suite
passed 33 tests with 11 platform-specific skips; the Rust
suite passed 8 tests. The physical-desktop checks below remain necessary.

For release acceptance on your GNOME 50 machine, verify automatic paste into GTK and browser
text fields while the window is hidden; denied/revoked permissions; hold/double-tap/cancel;
camera restart; whisper microphone capture; practice/training; and login autostart. Confirm
`doctor` selects the RTX 3080 and that the runtime has `pipewiresrc`. The cloud environment has
no real GNOME session, camera, microphone, or NVIDIA device.

The committed `python-deps.json` and `cargo-sources.json` allow Flatpak builds without a native
development setup. When updating the Python/Rust locks, regenerate them on Linux x86_64 from
the frozen Python 3.12 environment (including safetensors):

```sh
cd linux
cargo run --locked -- flatpak-sources
```

The generator selects compatible wheels and their locked SHA-256 hashes, resolves only the
same pinned safetensors version from PyPI, and emits Cargo archive sources with lockfile
checksums. macOS/Windows input-hook dependencies and Python test packages are excluded from
the Flatpak because the Rust frontend owns those desktop functions.
