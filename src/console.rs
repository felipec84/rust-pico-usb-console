// ─── Consola USB-CDC ────────────────────────────────────────────────────────
// Todo lo que hace correr la consola de comandos: las tareas de transporte
// (usb_task, serial_task) y la lógica de la aplicación (app_task), más los
// canales que las conectan entre sí. `main.rs` solo arma el hardware (USB,
// flash, ADC) y lanza estas tareas — la sustancia vive acá.

use core::fmt::Write as _;

use embassy_rp::peripherals::USB;
use embassy_rp::rom_data;
use embassy_rp::usb::Driver;
use embassy_rp::watchdog::ResetReason;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer, with_timeout};
use embassy_usb::class::cdc_acm::CdcAcmClass;

// ─── Canales de comunicación entre tareas ──────────────────────────────────
// RX: líneas de comando recibidas por USB-CDC, de serial_task a app_task.
// TX: respuestas ya formateadas, de app_task de vuelta a serial_task.
//
// TX_CHANNEL tiene profundidad 32, no 4: con try_send() y una cola corta,
// cualquier comando que responda en más líneas de las que caben en el buffer
// (p. ej. un futuro "dump" de varias líneas) pierde las líneas que no
// caben, en silencio — try_send() descarta si el canal está lleno, no
// bloquea ni avisa. 32 da margen para respuestas multilínea razonables sin
// tener que auditar cada comando nuevo para ver si cabe.
static RX_CHANNEL: Channel<ThreadModeRawMutex, heapless::Vec<u8, 64>, 4> = Channel::new();
pub(crate) static TX_CHANNEL: Channel<ThreadModeRawMutex, heapless::String<200>, 32> =
    Channel::new();

// ─── Máscara de interfaces para el modo BOOTSEL ────────────────────────────
//
// Segundo argumento de `rom_data::reset_to_usb_boot`. Bit 0 deshabilita la
// interfaz USB Mass Storage (el disco RPI-RP2); bit 1 deshabilitaría PICOBOOT.
// Dejamos PICOBOOT viva porque es la que usa picotool — el disco no lo usa
// nadie en este flujo de trabajo.
//
// Por qué apagar el disco (MEDIDO en Ubuntu 24.04, 2026-08-03): con la máscara
// en 0 el kernel engancha `usb-storage`, monta `/dev/sda1` (label RPI-RP2) y,
// cuando picotool termina de cargar y reinicia la placa, el disco desaparece
// en plena operación SCSI. El kernel registra entonces:
//
//   device offline error, dev sda, sector 260 op 0x1:(WRITE)
//   Buffer I/O error on dev sda1, logical block 259, lost async page write
//   FAT-fs (sda1): unable to read boot sector to mark fs as dirty
//
// Con la máscara en 1 no aparece ningún `sd*` ni una sola línea de SCSI, y
// picotool sigue funcionando igual. Es ruido evitable en `journalctl`, no la
// causa del bug de re-enumeración — ese era otro (ver
// usb-reenum-investigation.md).
const DISABLE_MSC: u32 = 1;

// pub(crate): la usa main() (número de serie USB, a partir del ID de la
// flash) además de app_task más abajo (comando "info").
pub(crate) fn hex_encode_upper(bytes: &[u8], out: &mut [u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for (i, b) in bytes.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0xf) as usize];
    }
}

// ─── Tarea 1: USB stack ────────────────────────────────────────────────────
#[embassy_executor::task]
pub async fn usb_task(mut usb: embassy_usb::UsbDevice<'static, Driver<'static, USB>>) {
    usb.run().await;
}

// ─── Tarea 2: Puerto serie CDC bidireccional ───────────────────────────────
#[embassy_executor::task]
pub async fn serial_task(
    mut class: CdcAcmClass<'static, Driver<'static, USB>>,
    panic_msg: Option<&'static str>,
) {
    let mut buf = [0u8; 64];
    let mut primer_boot = true; // enviar diagnóstico solo en la primera conexión
    let mut line: heapless::Vec<u8, 64> = heapless::Vec::new(); // línea de comando en construcción

    loop {
        // wait_connection() solo espera la enumeración USB (interfaz habilitada
        // por el host), NO que un programa abra el puerto. Para detectar la
        // apertura real hay que mirar DTR, que el kernel/pyserial levanta al
        // abrir /dev/ttyACM0. Sin esto, aperturas efímeras (ModemManager
        // sondeando el puerto, etc.) son invisibles para el firmware: el banner
        // y su eco se los lleva el primer proceso que abre el puerto, y la
        // basura recibida en esa sesión quedaba en `line` contaminando el
        // primer comando de la sesión real del usuario.
        class.wait_connection().await;
        while !class.dtr() {
            // La señal de reset por baud 1200 (flash.sh usa `stty -F ... 1200`)
            // llega en una apertura efímera del puerto que puede no levantar
            // DTR nunca — hay que chequearla también aquí, no solo dentro del
            // bucle de sesión. El line coding queda guardado en el estado CDC
            // aunque el puerto ya se haya cerrado.
            if class.line_coding().data_rate() == 1200 {
                Timer::after(Duration::from_millis(100)).await;
                rom_data::reset_to_usb_boot(0, DISABLE_MSC);
            }
            Timer::after(Duration::from_millis(20)).await;
        }

        // Frontera de sesión: descartar cualquier línea a medio escribir y
        // cualquier comando/respuesta pendiente de una conexión anterior.
        line.clear();
        while RX_CHANNEL.try_receive().is_ok() {}
        while TX_CHANNEL.try_receive().is_ok() {}

        // ── Enviar mensaje de pánico del boot anterior (si existe) ─────────
        //
        // Se envía solo en la primera conexión del boot actual. Si el host
        // se desconecta y reconecta, no se repite el mensaje.
        if primer_boot {
            primer_boot = false;
            if let Some(msg) = panic_msg {
                let _ = class.write_packet(b"\r\n").await;
                let _ = class
                    .write_packet("╔══════════════════════════════════════╗\r\n".as_bytes())
                    .await;
                let _ = class
                    .write_packet("║  !! PANIC EN BOOT ANTERIOR !!       ║\r\n".as_bytes())
                    .await;
                let _ = class
                    .write_packet("╚══════════════════════════════════════╝\r\n".as_bytes())
                    .await;

                // Enviar el mensaje en chunks de 64 bytes (límite del paquete CDC)
                for chunk in msg.as_bytes().chunks(64) {
                    let _ = class.write_packet(chunk).await;
                }

                let _ = class.write_packet(b"\r\n").await;
                let _ = class
                    .write_packet("════════════════════════════════════════\r\n".as_bytes())
                    .await;
                let _ = class
                    .write_packet(b"Sistema operando normalmente.\r\n\r\n")
                    .await;
            }
        }

        // Banner corto en CADA apertura del puerto (no solo la primera): si un
        // proceso efímero del host (ModemManager sondeando) abre el puerto
        // antes que el usuario, no se "roba" el único banner del boot.
        let _ = class.write_packet(crate::BANNER).await;

        // ── Purga post-apertura ────────────────────────────────────────────
        // Al abrir /dev/ttyACM0 hay una ventana breve, antes de que el
        // programa de terminal configure el modo raw, en la que la disciplina
        // de línea del kernel todavía tiene ECHO activo: bytes que enviemos en
        // esa ventana (el banner) vuelven "tecleados" hacia la Pico. Como no
        // traen \r, quedaban acumulados en `line` y se pegaban como prefijo
        // del primer comando real ("comando desconocido" pese a teclearlo
        // bien). Se descarta todo lo recibido hasta que la línea quede en
        // silencio (máx ~300 ms) — cubre ese eco del tty, sondas tipo
        // ModemManager y cualquier dato residual del driver USB.
        let drain_deadline = embassy_time::Instant::now() + Duration::from_millis(300);
        while embassy_time::Instant::now() < drain_deadline {
            match with_timeout(Duration::from_millis(50), class.read_packet(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {} // eco/basura: descartar y seguir purgando
                Err(_) => break,         // 50 ms de silencio: línea limpia
                _ => break,              // error de lectura o paquete vacío
            }
        }
        line.clear();

        // ── Bucle de comunicación bidireccional ────────────────────────────
        loop {
            // Puerto cerrado (DTR abajo) → fin de sesión. read_packet NO
            // devuelve error cuando el host simplemente cierra el puerto (solo
            // cuando el USB se des-configura), así que sin este chequeo el
            // firmware nunca notaba cierres/reaperturas del puerto.
            if !class.dtr() {
                break;
            }

            // Detección de baud 1200 → reset a BOOTSEL (para reprogramar).
            let coding = class.line_coding();
            if coding.data_rate() == 1200 {
                Timer::after(Duration::from_millis(100)).await;
                // Reboot al modo BOOTSEL del RP2040 (ROM function)
                rom_data::reset_to_usb_boot(0, DISABLE_MSC);
            }

            // Recibir datos desde el host con un timeout para poder chequear el baud rate periódicamente
            match with_timeout(Duration::from_millis(50), class.read_packet(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    // Eco en un solo write_packet por paquete recibido (igual que el
                    // echo original) en vez de uno por byte — varios write_packet
                    // pequeños seguidos producían corrupción intermitente en el
                    // siguiente read_packet durante pruebas en hardware real.
                    let _ = class.write_packet(&buf[..n]).await;

                    // Consola de línea: acumula bytes hasta \r/\n, con soporte de
                    // backspace (se corrige visualmente con un write_packet aparte,
                    // ya que el eco en bloque de arriba solo mueve el cursor). No
                    // interpreta secuencias de escape ANSI (flechas, etc. entran
                    // como bytes sueltos en la línea) — alcanza para una consola
                    // simple tipo esqueleto. (El eco del tty del host durante la
                    // apertura del puerto, que ensuciaba el primer comando, se
                    // purga tras el banner — ver "Purga post-apertura" arriba.)
                    for &b in &buf[..n] {
                        match b {
                            b'\r' | b'\n' => {
                                if !line.is_empty() {
                                    let _ = RX_CHANNEL.try_send(line.clone());
                                    line.clear();
                                }
                            }
                            0x08 | 0x7F => {
                                if line.pop().is_some() {
                                    let _ = class.write_packet(b" \x08").await;
                                }
                            }
                            _ => {
                                let _ = line.push(b);
                            }
                        }
                    }
                }
                Ok(Err(_)) => break, // host cerró el puerto → volver a wait_connection
                _ => {}              // Timeout o paquete de tamaño 0
            }

            // Enviar cualquier respuesta pendiente de app_task, terminada en
            // \r\n para que el eco del siguiente comando empiece en línea nueva
            // (las respuestas en app_task no llevan salto de línea final).
            while let Ok(resp) = TX_CHANNEL.try_receive() {
                for chunk in resp.as_bytes().chunks(64) {
                    let _ = class.write_packet(chunk).await;
                }
                let _ = class.write_packet(b"\r\n").await;
            }
        }
    }
}

// ─── Tarea 3: Consola de comandos / lógica de la aplicación ────────────────
// Espera líneas de comando de serial_task (vía RX_CHANNEL) y responde por
// TX_CHANNEL. Los comandos de abajo (help/info/temp/uptime/bootsel) son un
// ejemplo — reemplázalos por los de tu proyecto en este mismo match.
//
// Nota sobre "temp": lee sensors::get_status(), NO toca el ADC acá. El ADC es
// propiedad exclusiva de sensors_task (ver sensors.rs) — este patrón es el
// que hay que copiar para cualquier sensor propio, en particular si su
// lectura es lenta (I2C, 1-Wire): la lectura ocurre en la tarea dueña del
// periférico, la consola solo lee el último valor cacheado.
//
// Si en cambio necesitas que un comando DISPARE una acción en un módulo que
// posee un bus exclusivo (no solo leer su último estado), usa una petición
// RPC por `embassy_sync::signal::Signal` en vez de pasarle el periférico a
// esta tarea: un `Signal<_, ()>` de pedido y un `Signal<_, T>` de respuesta,
// con una función async `pub async fn hacer_algo() -> T` en el módulo dueño
// que hace `REQUEST.signal(()); RESULT.wait().await`. El módulo dueño queda
// esperando ese Signal junto a su loop periódico (con `select()`/`select3()`
// de embassy-futures). Este esqueleto no trae un ejemplo concreto porque no
// tiene un bus compartido por defecto — agrégalo cuando tengas uno.
#[embassy_executor::task]
pub async fn app_task(uid: [u8; 8], reset_reason: Option<ResetReason>) {
    loop {
        let msg = RX_CHANNEL.receive().await;
        let mut resp: heapless::String<200> = heapless::String::new();

        match msg.as_slice() {
            b"help" => {
                let _ = write!(resp, "Comandos: help, info, temp, uptime, bootsel");
            }
            b"info" => {
                let mut hex = [0u8; 16];
                hex_encode_upper(&uid, &mut hex);
                let hex_str = core::str::from_utf8(&hex).unwrap_or("????????????????");
                let reason = match reset_reason {
                    Some(ResetReason::Forced) => "forced (watchdog trigger_reset)",
                    Some(ResetReason::TimedOut) => "watchdog timeout",
                    None => "power-on o soft-reset (el RP2040 no distingue estos casos)",
                };
                // Se manda en DOS mensajes a propósito: `resp` es una
                // heapless::String<200> y `write!` trunca en silencio al
                // llenarse. Con la línea de procedencia (un `git describe` con
                // tags puede ser largo) el bloque completo rozaba el límite, y
                // perder el hash por truncamiento silencioso es justo lo que no
                // queremos de un dato de trazabilidad.
                let mut head: heapless::String<200> = heapless::String::new();
                let _ = write!(
                    head,
                    "{} v{}\r\nFlash UID: {}\r\nUltimo reset: {}\r\nWatchdog boot count: {}",
                    crate::PRODUCT_NAME,
                    env!("CARGO_PKG_VERSION"),
                    hex_str,
                    reason,
                    crate::watchdog::boot_count(),
                );
                let _ = TX_CHANNEL.send(head).await;

                // Procedencia del binario. GIT_DESCRIBE/GIT_COMMIT_DATE los
                // inyecta build.rs en tiempo de compilación — ver el comentario
                // largo allá. Un sufijo "-dirty" significa que este binario NO
                // salió de un commit limpio: el hash no lo reconstruye.
                let _ = write!(
                    resp,
                    "Firmware: {} ({})",
                    env!("GIT_DESCRIBE"),
                    env!("GIT_COMMIT_DATE"),
                );
            }
            b"temp" => {
                let status = crate::sensors::get_status();
                let _ = write!(
                    resp,
                    "Temperatura interna: {:.1} C (raw={})",
                    status.temperature_c, status.raw_temp
                );
            }
            b"uptime" => {
                let ms = embassy_time::Instant::now().as_millis();
                let _ = write!(resp, "Uptime: {} ms", ms);
            }
            b"bootsel" => {
                let _ = write!(resp, "Reiniciando a BOOTSEL...");
                let _ = TX_CHANNEL.send(resp).await;
                Timer::after(Duration::from_millis(100)).await;
                rom_data::reset_to_usb_boot(0, DISABLE_MSC);
                continue;
            }
            _ => {
                let _ = write!(resp, "Comando desconocido. Escribe 'help'.");
            }
        }

        let _ = TX_CHANNEL.send(resp).await;
    }
}
