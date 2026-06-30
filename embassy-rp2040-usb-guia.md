# Embassy + Rust + Pico RP2040
## Guía completa: USB CDC bidireccional con reprogramación via picotool y panic-persist

> Válida para **Ubuntu 24.04** y **Raspberry Pi 4 (RPiOS Bookworm)**  
> Sin probe de debug — solo cable USB

---

## Índice

1. [Instalar herramientas del sistema](#1-instalar-herramientas-del-sistema)
2. [Instalar Rust + target ARM](#2-instalar-rust--target-arm)
3. [Instalar picotool](#3-instalar-picotool)
4. [Configurar udev rules (sin sudo)](#4-configurar-udev-rules-sin-sudo)
5. [Crear el proyecto con cargo](#5-crear-el-proyecto-con-cargo)
6. [Estructura de archivos del proyecto](#6-estructura-de-archivos-del-proyecto)
7. [Código fuente completo](#7-código-fuente-completo)
8. [Configurar VSCode](#8-configurar-vscode)
9. [Flujo de trabajo diario](#9-flujo-de-trabajo-diario)
10. [Cómo funciona el reset automático](#10-cómo-funciona-el-reset-automático)
11. [Notas sobre VID:PID 2E8A:000A](#11-notas-sobre-vidpid-2e8a000a)

---

## 1. Instalar herramientas del sistema

Ejecuta esto **igual** en Ubuntu 24.04 y en la Raspberry Pi 4:

```bash
sudo apt update && sudo apt install -y \
    git \
    curl \
    build-essential \
    pkg-config \
    libusb-1.0-0-dev \
    libudev-dev \
    cmake \
    flip-link
```

> **En RPi4:** `flip-link` puede no estar en apt. Instálalo luego vía cargo (paso 2).

---

## 2. Instalar Rust + target ARM

```bash
# Instalar rustup (si no lo tienes)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Target para Cortex-M0+ (RP2040)
rustup target add thumbv6m-none-eabi

# Herramientas de desarrollo embebido
cargo install flip-link          # protección contra stack overflow
cargo install elf2uf2-rs         # convierte .elf → .uf2 (y calcula el checksum de boot2)
cargo install cargo-generate     # para crear proyectos desde templates (opcional)
```

Verifica:
```bash
rustc --version        # >= 1.75
rustup show            # debe mostrar thumbv6m-none-eabi
```

---

## 3. Instalar picotool

`picotool` es la herramienta de Raspberry Pi para flashear y reiniciar la Pico sin tocar botones.

```bash
# Dependencias
sudo apt install -y cmake libusb-1.0-0-dev

# Clonar y compilar
cd ~
git clone https://github.com/raspberrypi/picotool.git
cd picotool
mkdir build && cd build
cmake .. -DCMAKE_INSTALL_PREFIX=/usr/local
make -j$(nproc)
sudo make install

# Verificar
picotool version
```

---

## 4. Configurar udev rules (sin sudo)

Sin estas reglas, `picotool` y el acceso a los puertos serie requieren `sudo` en cada uso.

```bash
# Copiar las reglas oficiales de picotool
sudo cp ~/picotool/udev/60-picotool.rules /etc/udev/rules.d/

# Regla adicional para el puerto serie CDC (VID:PID 2E8A:000A)
sudo tee /etc/udev/rules.d/61-pico-cdc.rules << 'EOF'
# Pico en modo normal (CDC serial) - VID:PID 2E8A:000A
SUBSYSTEM=="tty", ATTRS{idVendor}=="2e8a", ATTRS{idProduct}=="000a", \
    MODE="0666", SYMLINK+="ttyPICO"

# Pico en modo BOOTSEL
SUBSYSTEM=="usb", ATTRS{idVendor}=="2e8a", ATTRS{idProduct}=="0003", \
    MODE="0666"
EOF

# Agregar tu usuario al grupo dialout y plugdev
sudo usermod -aG dialout $USER
sudo usermod -aG plugdev $USER

# Recargar udev
sudo udevadm control --reload-rules
sudo udevadm trigger

# IMPORTANTE: cerrar sesión y volver a entrar para que los grupos tomen efecto
# (o usar: newgrp dialout)
```

---

## 5. Crear el proyecto con cargo

```bash
cd ~
mkdir -p proyectos-embebidos
cd proyectos-embebidos

# Crear proyecto Rust
cargo new pico-usb-console --name pico_usb_console
cd pico-usb-console
```

---

## 6. Estructura de archivos del proyecto

```
pico-usb-console/
├── .cargo/
│   └── config.toml          ← runner y target ARM
├── src/
│   └── main.rs              ← código principal
├── memory.x                 ← mapa de memoria del RP2040 (con .boot2 y PANDUMP)
├── build.rs                 ← script de build
├── Cargo.toml               ← dependencias (Embassy + panic-persist)
├── .vscode/
│   ├── tasks.json           ← tareas de build/flash
│   └── extensions.json      ← extensiones recomendadas
└── flash.sh                 ← script de reprogramación automática (5 pasos)
```

---

## 7. Código fuente completo

### `Cargo.toml`

```toml
[package]
name = "pico_usb_console"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "pico_usb_console"
path = "src/main.rs"

[dependencies]
embassy-executor = { version = "0.10.0", git = "https://github.com/embassy-rs/embassy", features = [
    "platform-cortex-m", "executor-thread", "executor-interrupt"
]}
embassy-rp = { version = "0.10.0", git = "https://github.com/embassy-rs/embassy", features = [
    "unstable-pac", "time-driver", "critical-section-impl", "rp2040", "binary-info"
]}

embassy-usb  = { version = "0.6.0", git = "https://github.com/embassy-rs/embassy" }
embassy-time = { version = "0.5.1", git = "https://github.com/embassy-rs/embassy" }
embassy-sync = { version = "0.8.0", git = "https://github.com/embassy-rs/embassy" }

cortex-m    = { version = "0.7", features = ["inline-asm"] }
cortex-m-rt = "0.7"
critical-section = "1.1"
static_cell = "2"
portable-atomic = { version = "1.5", features = ["critical-section"] }
heapless    = "0.9"

# panic-persist: guarda el mensaje de pánico en RAM (zona PANDUMP, definida
# en memory.x) y hace un soft-reset. En el siguiente boot el firmware lee
# ese mensaje y lo envía por USB-CDC al conectarse el host.
panic-persist = { version = "0.3", features = ["utf8"] }

[profile.release]
debug         = 2      # conservar símbolos para análisis post-mortem
opt-level     = "s"    # optimizar por tamaño de flash
lto           = true
codegen-units = 1
```

### `.cargo/config.toml`

```toml
[build]
target = "thumbv6m-none-eabi"

[target.thumbv6m-none-eabi]
# Runner por defecto. Nota: En Linux, si la Pico no se auto-monta al
# entrar en BOOTSEL, elf2uf2-rs --deploy fallará.
# Para un flujo de flasheo robusto y automatizado, usa ./flash.sh.
runner = "elf2uf2-rs --deploy --serial"

rustflags = [
    "-C", "link-arg=--nmagic",
    "-C", "link-arg=-Tlink.x",
    "-C", "linker=flip-link",
]
```

### `memory.x`

> **IMPORTANTE:** Debes mapear explícitamente la sección `.boot2` al bloque `BOOT2` al principio de la flash. De lo contrario, el enlazador la colocará en otra parte y la Pico no arrancará (se quedará en un bucle de BOOTSEL).

```ld
MEMORY {
    BOOT2   : ORIGIN = 0x10000000, LENGTH = 0x100
    FLASH   : ORIGIN = 0x10000100, LENGTH = 2048K - 0x100
    RAM     : ORIGIN = 0x20000000, LENGTH = 255K
    PANDUMP : ORIGIN = 0x2003FC00, LENGTH = 1K
}

/* Símbolos requeridos por panic-persist para encontrar la zona */
_panic_dump_start = ORIGIN(PANDUMP);
_panic_dump_end   = ORIGIN(PANDUMP) + LENGTH(PANDUMP);

SECTIONS {
    /* Ubicar la segunda etapa del bootloader al principio de la flash */
    .boot2 : {
        KEEP(*(.boot2));
    } > BOOT2

    .bi_entries : ALIGN(4) {
        __bi_entries_start = .;
        KEEP(*(.bi_entries));
        . = ALIGN(4);
        __bi_entries_end = .;
    } > FLASH
} INSERT AFTER .text;
```

### `build.rs`

```rust
fn main() {
    println!("cargo:rerun-if-changed=memory.x");
}
```

### `src/main.rs`

```rust
#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::USB;
use embassy_rp::rom_data;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer, with_timeout};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::{Builder, Config};
use static_cell::StaticCell;

// panic-persist: maneja los pánicos escribiendo el mensaje en la zona PANDUMP.
// No uses panic-halt ni panic-reset junto con este crate.
use panic_persist as _;

// Enlazar interrupciones USB
bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

// Canal para comunicación entre tareas (PC -> Lógica)
static RX_CHANNEL: Channel<ThreadModeRawMutex, heapless::Vec<u8, 64>, 4> = Channel::new();

// Buffers estáticos para embassy-usb 0.6
static STATE: StaticCell<State> = StaticCell::new();
static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
static BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
static MSOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // 1. Leer mensaje de pánico del boot anterior antes de tocar la RAM
    let panic_msg: Option<&'static str> = panic_persist::get_panic_message_utf8();

    // 2. Inicializar hardware
    let p = embassy_rp::init(Default::default());
    let driver = Driver::new(p.USB, Irqs);

    // 3. Configurar USB (VID:PID 2E8A:000A oficial de Raspberry Pi para CDC)
    let mut config = Config::new(0x2E8A, 0x000A);
    config.manufacturer = Some("Raspberry Pi");
    config.product = Some("Pico USB Console");
    config.serial_number = Some("ECODITEC001");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    // Construir el dispositivo USB (Embassy 0.6 ya no requiere pasar el device descriptor)
    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESCRIPTOR.init([0; 256]),
        BOS_DESCRIPTOR.init([0; 256]),
        MSOS_DESCRIPTOR.init([0; 256]),
        CONTROL_BUF.init([0; 64]),
    );

    let state = STATE.init(State::new());
    let class = CdcAcmClass::new(&mut builder, state, 64);
    let usb = builder.build();

    // 4. Lanzar tareas concurrentes
    spawner.spawn(usb_task(usb).unwrap()).unwrap();
    spawner.spawn(serial_task(class, panic_msg).unwrap()).unwrap();
    spawner.spawn(app_task().unwrap()).unwrap();
}

#[embassy_executor::task]
async fn usb_task(mut usb: embassy_usb::UsbDevice<'static, Driver<'static, USB>>) {
    usb.run().await;
}

#[embassy_executor::task]
async fn serial_task(
    mut class: CdcAcmClass<'static, Driver<'static, USB>>,
    panic_msg: Option<&'static str>,
) {
    let mut buf = [0u8; 64];
    let mut primer_boot = true;

    loop {
        class.wait_connection().await;

        // Enviar reporte de pánico si existió uno
        if primer_boot {
            primer_boot = false;
            if let Some(msg) = panic_msg {
                let _ = class.write_packet(b"\r\n╔══════════════════════════════════════╗\r\n").await;
                let _ = class.write_packet(b"║  !! PANIC EN BOOT ANTERIOR !!       ║\r\n").await;
                let _ = class.write_packet(b"╚══════════════════════════════════════╝\r\n").await;
                for chunk in msg.as_bytes().chunks(64) {
                    let _ = class.write_packet(chunk).await;
                }
                let _ = class.write_packet(b"\r\n════════════════════════════════════════\r\n").await;
            } else {
                let _ = class.write_packet(b"[Pico USB Console - boot limpio]\r\n").await;
            }
        }

        loop {
            // Detección de baud 1200 -> reset a BOOTSEL
            let coding = class.line_coding();
            if coding.data_rate() == 1200 {
                Timer::after(Duration::from_millis(100)).await;
                rom_data::reset_to_usb_boot(0, 0);
            }

            // Leemos con timeout de 50ms para evitar bloquear indefinidamente
            // y permitir que el bucle evalúe el cambio de baudrate a 1200
            match with_timeout(Duration::from_millis(50), class.read_packet(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    let data = &buf[..n];
                    let _ = class.write_packet(data).await; // Eco

                    let mut msg: heapless::Vec<u8, 64> = heapless::Vec::new();
                    let _ = msg.extend_from_slice(data);
                    let _ = RX_CHANNEL.try_send(msg);
                }
                Ok(Err(_)) => break, // Desconexión
                _ => {} // Timeout o paquete vacío
            }
        }
    }
}

#[embassy_executor::task]
async fn app_task() {
    loop {
        while let Ok(msg) = RX_CHANNEL.try_receive() {
            let _ = msg; // Procesar comandos aquí
        }
        Timer::after(Duration::from_millis(500)).await;
    }
}
```

### `flash.sh`

```bash
#!/usr/bin/env bash
# Reprograma la Pico sin tocar botones físicos.
# Requiere: cargo, picotool, elf2uf2-rs, stty (coreutils)

set -euo pipefail

BINARY="${1:-target/thumbv6m-none-eabi/release/pico_usb_console}"
SERIAL="${PICO_PORT:-/dev/ttyACM0}"
MAX_WAIT=10

echo "════════════════════════════════════"
echo " Pico Flash Script"
echo "════════════════════════════════════"

echo "▶ [1/5] Compilando firmware (release)..."
cargo build --release
echo "   OK: $BINARY"

echo "▶ [2/5] Convirtiendo a UF2 con elf2uf2-rs..."
# elf2uf2-rs calcula automáticamente el checksum requerido en boot2
elf2uf2-rs "$BINARY" "${BINARY}.uf2"

if [ -e "$SERIAL" ]; then
    echo "▶ [3/5] Enviando señal de reset (baud 1200) a $SERIAL ..."
    stty -F "$SERIAL" 1200 2>/dev/null || true
    sleep 1.5
else
    echo "▶ [3/5] Puerto $SERIAL no encontrado — esperando BOOTSEL manual..."
    echo "   (En el primer flash: mantén BOOTSEL y conecta el USB)"
fi

echo "▶ [4/5] Esperando dispositivo en modo BOOTSEL..."
FOUND=0
for i in $(seq 1 $MAX_WAIT); do
    # Usar el código de salida de picotool directamente (es compatible con RP2040, RP2350, etc.)
    if picotool info >/dev/null 2>&1; then
        FOUND=1
        break
    fi
    echo -n "."
    sleep 1
done
echo ""

if [ $FOUND -eq 0 ]; then
    echo "✗ ERROR: No se encontró la Pico en modo BOOTSEL tras ${MAX_WAIT}s."
    echo "  - Primer flash: conecta con BOOTSEL presionado"
    echo "  - Si es un reflash: verifica que el firmware usa panic-persist"
    echo "    (panic-halt congela el USB y requiere el botón físico)"
    exit 1
fi

echo "▶ [5/5] Cargando firmware con picotool..."
# Flasheamos el archivo .uf2 (que tiene el checksum correcto) y reiniciamos (-x)
picotool load "${BINARY}.uf2" -f -x
echo ""
echo "✅ Listo. La Pico está reiniciando."
echo "   Monitor: python3 -m serial.tools.miniterm $SERIAL 115200"
```

Hacer ejecutable:
```bash
chmod +x flash.sh
```

---

## 8. Configurar VSCode

### `.vscode/extensions.json`

```json
{
  "recommendations": [
    "rust-lang.rust-analyzer",
    "probe-rs.probe-rs-debugger",
    "ms-vscode.hexeditor",
    "serayuzgur.crates",
    "tamasfe.even-better-toml"
  ]
}
```

### `.vscode/tasks.json`

```json
{
  "version": "2.0.0",
  "tasks": [
    {
      "label": "Build (release)",
      "type": "shell",
      "command": "cargo build --release",
      "group": { "kind": "build", "isDefault": true },
      "problemMatcher": ["$rustc"]
    },
    {
      "label": "Flash (automático)",
      "type": "shell",
      "command": "./flash.sh",
      "group": "build",
      "dependsOn": "Build (release)",
      "problemMatcher": []
    },
    {
      "label": "Monitor USB Serial",
      "type": "shell",
      "command": "python3 -m serial.tools.miniterm /dev/ttyACM0 115200 --eol CRLF",
      "group": "test",
      "problemMatcher": []
    }
  ]
}
```

### `.vscode/settings.json`

```json
{
  "rust-analyzer.cargo.target": "thumbv6m-none-eabi",
  "rust-analyzer.cargo.features": "all",
  "rust-analyzer.checkOnSave.extraArgs": [
    "--target", "thumbv6m-none-eabi"
  ],
  "[rust]": {
    "editor.formatOnSave": true
  }
}
```

---

## 9. Flujo de trabajo diario

### Primera vez (o si la Pico está vacía)
1. Mantén presionado el botón **BOOTSEL** de la Pico mientras conectas el cable USB a la PC.
2. Ejecuta el script de flasheo:
   ```bash
   ./flash.sh
   ```
3. El script detectará la Pico en modo de arranque, generará el archivo `.uf2` firmado y lo subirá usando `picotool`. La Pico se reiniciará automáticamente al terminar.

### Siguientes reprogramaciones (100% automático)
Simplemente ejecuta:
```bash
./flash.sh
```
O presiona `Ctrl+Shift+B` en VSCode y selecciona **Flash (automático)**. El script se encargará de mandar la señal a `1200` baudios, esperar a que la Pico entre en BOOTSEL, compilar, y flashearla de nuevo sin que tengas que tocar un solo botón físico.

---

## 10. Cómo funciona el reset automático

```
PC                               Pico (Embassy)
 |                                    |
 |  Abre /dev/ttyACM0 a 1200 bps      |
 |───────────────────────────────────>|
 |                           with_timeout descompila la tarea
 |                           Detecta coding.data_rate() == 1200
 |                           Llama rom_data::reset_to_usb_boot(0,0)
 |                                    |
 |  Pico desaparece como ACM          |
 |<───────────────────────────────────|
 |                           Pico aparece en modo BOOTSEL (USB raw)
 |                                    |
 |  picotool load firmware.uf2 -f -x  |
 |───────────────────────────────────>|
 |                           Se graba la flash y se reinicia
 |<───────────────────────────────────|
 |  /dev/ttyACM0 aparece de nuevo     |
```

---

## 11. Notas sobre VID:PID 2E8A:000A

`2E8A:000A` es el par de identificadores oficiales de Raspberry Pi asignados para periféricos USB CDC (puertos serie virtuales). Es detectado automáticamente por `picotool` y el kernel Linux para cargar los drivers de puerto serie genéricos (`cdc_acm`).

Si decides cambiar de VID:PID en un futuro, ten en cuenta que el reset automático por software (baudrate 1200) seguirá funcionando porque se maneja en el firmware, pero es posible que tengas que configurar udev rules específicas para tu nuevo VID:PID.
