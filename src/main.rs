#![no_std]
#![no_main]

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_rp::adc; // qualificado a propósito: adc::Channel/InterruptHandler
                      // colisionan de nombre con embassy_sync::channel::Channel
                      // y embassy_rp::usb::InterruptHandler.
use embassy_rp::bind_interrupts;
use embassy_rp::flash::{Blocking, Flash};
use embassy_rp::peripherals::USB;
use embassy_rp::rom_data;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_rp::watchdog::{ResetReason, Watchdog};
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer, with_timeout};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::control::{OutResponse, Recipient, Request};
use embassy_usb::types::InterfaceNumber;
use embassy_usb::{Builder, Config, Handler};
use static_cell::StaticCell;

// panic-persist: en caso de pánico, escribe el mensaje en la zona PANDUMP
// (definida en memory.x) y hace un soft-reset. El USB-CDC vuelve a estar
// disponible en el siguiente boot. No uses panic-halt ni panic-reset junto
// con este crate — panic-persist ya incluye su propio #[panic_handler].
use panic_persist as _;

// ─── Identidad del producto ────────────────────────────────────────────────
// CUSTOMIZE PER PROJECT: nombre visible en lsusb/picotool y en el banner.
// Los asserts se evalúan EN COMPILACIÓN — un nombre demasiado largo aquí no
// compila, en vez de hacer que embassy-usb entre en pánico serializando el
// string descriptor durante la enumeración (síntoma: la Pico se resetea en
// bucle y el host registra "can't set config #1, error -32").
const PRODUCT_NAME: &str = "Pico USB Console";
const BANNER: &[u8] = b"[Pico USB Console - escribe 'help']\r\n";

// Límite del spec USB: bLength del string descriptor es un u8 → máximo
// 126 unidades UTF-16. Con nombres ASCII, bytes == unidades.
const _: () = assert!(
    PRODUCT_NAME.len() <= 126,
    "PRODUCT_NAME excede los 126 caracteres del string descriptor USB"
);
// write_packet envía UN paquete CDC: máximo 64 bytes o el banner no sale.
const _: () = assert!(
    BANNER.len() <= 64,
    "El banner no cabe en un paquete CDC de 64 bytes — acorta el nombre"
);

// ─── Metadata para picotool ────────────────────────────────────────────────
#[unsafe(link_section = ".bi_entries")]
#[cfg(target_os = "none")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 3] = [
    embassy_rp::binary_info::rp_program_name!(c"Pico USB Console"),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_description!(c"USB CDC Console with Auto-Reset"),
];

// ─── Interrupciones ────────────────────────────────────────────────────────
bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

// ─── Canales de comunicación entre tareas ──────────────────────────────────
// RX: líneas de comando recibidas por USB-CDC, de serial_task a app_task.
// TX: respuestas ya formateadas, de app_task de vuelta a serial_task.
static RX_CHANNEL: Channel<ThreadModeRawMutex, heapless::Vec<u8, 64>, 4> = Channel::new();
static TX_CHANNEL: Channel<ThreadModeRawMutex, heapless::String<200>, 4> = Channel::new();

// ─── Handler de reset para picotool (-f / --force) ────────────────────────
//
// picotool busca una interfaz USB con class=0xFF subclass=0x00 proto=0x01 y,
// para rebootear, envía una petición de control con:
//   bmRequestType = CLASS | INTERFACE   (NO vendor — ver picotool main.cpp)
//   bRequest      = 1 (RESET_REQUEST_BOOTSEL) ó 2 (RESET_REQUEST_FLASH)
//   wIndex        = número de la interfaz de reset
//
// El driver de referencia de la pico-sdk (reset_interface.c) NO comprueba el
// tipo de la petición: solo mira wIndex y bRequest. Por eso aquí basta con
// verificar el recipient (Interface) y el índice, sin exigir un tipo concreto
// — así funciona tanto si picotool la envía como CLASS como VENDOR.
struct PicotoolResetHandler {
    if_num: InterfaceNumber,
}

impl PicotoolResetHandler {
    // ¿Esta petición va dirigida a nuestra interfaz de reset?
    fn is_for_us(&self, req: &Request) -> bool {
        req.recipient == Recipient::Interface && req.index == u8::from(self.if_num) as u16
    }
}

impl Handler for PicotoolResetHandler {
    fn control_out(&mut self, req: Request, _data: &[u8]) -> Option<OutResponse> {
        if self.is_for_us(&req) {
            // bRequest=1 → RESET_REQUEST_BOOTSEL (reboot to BOOTSEL/UF2)
            // bRequest=2 → RESET_REQUEST_FLASH   (reboot normally)
            // Ambas funciones no retornan: reinician el chip de inmediato.
            //
            // disable_interface_mask=1: deshabilita la interfaz de almacenamiento
            // masivo (RPI-RP2) en BOOTSEL, dejando solo PICOBOOT — que es lo único
            // que picotool usa. Con ambas interfaces habilitadas, el SO monta el
            // drive RPI-RP2 y ese automount puede retrasar la re-enumeración lo
            // suficiente como para que picotool agote sus reintentos.
            if req.request == 1 {
                rom_data::reset_to_usb_boot(0, 1);
            } else if req.request == 2 {
                cortex_m::peripheral::SCB::sys_reset();
            }
            return Some(OutResponse::Accepted);
        }
        None
    }
}

static RESET_HANDLER: StaticCell<PicotoolResetHandler> = StaticCell::new();

// Tamaño físico de la flash (Winbond/QSPI en la Pico), no el tamaño usado
// por el linker en memory.x — necesario para el driver Flash de embassy-rp.
const FLASH_SIZE: usize = 2 * 1024 * 1024;

// picotool identifica, en modo BOOTSEL, al RP2040 por su ID único de flash
// (picoboot_connection.c: para RP2040 compara el flash ID vía PICOBOOT, NO
// el string de serie USB). Para que `picotool -f` pueda re-encontrar el
// dispositivo tras el reboot, el serial USB en modo normal debe ser ese
// mismo ID en hex (igual que hace pico-sdk con pico_get_unique_board_id()).
// Un serial arbitrario como "ECODITEC001" hace que picotool nunca reconozca
// el dispositivo reiniciado y agote sus reintentos, aunque el reboot en sí
// funcione.
static SERIAL_BUF: StaticCell<[u8; 16]> = StaticCell::new();

fn hex_encode_upper(bytes: &[u8], out: &mut [u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for (i, b) in bytes.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0xf) as usize];
    }
}

// Fórmula de calibración del sensor de temperatura interno (RP2040 datasheet §4.9.5).
fn convert_to_celsius(raw_temp: u16) -> f32 {
    let temp = 27.0 - (raw_temp as f32 * 3.3 / 4096.0 - 0.706) / 0.001721;
    let sign = if temp < 0.0 { -1.0 } else { 1.0 };
    let rounded_temp_x10: i16 = ((temp * 10.0) + 0.5 * sign) as i16;
    (rounded_temp_x10 as f32) / 10.0
}

// ─── Buffers estáticos para embassy-usb (StaticCell = sin unsafe) ──────────
static STATE: StaticCell<State> = StaticCell::new();
static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
static BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
static MSOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
// CONTROL_BUF: 256 y no 64. Los string descriptors (manufacturer/product/
// serial) se serializan a UTF-16 dentro de este buffer; embassy-usb hace
// assert de que caben, así que con 64 bytes un product de más de 30
// caracteres provoca pánico en plena enumeración (reboot-loop, el host
// nunca logra configurar el dispositivo). 256 cubre el máximo del spec.
static CONTROL_BUF: StaticCell<[u8; 256]> = StaticCell::new();

// ─── Punto de entrada ──────────────────────────────────────────────────────
#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // ── PASO 1: Leer mensaje de pánico ANTES de inicializar nada ──────────
    //
    // panic-persist guarda el mensaje en la zona PANDUMP de RAM. Hay que
    // leerlo aquí, en el primer instante del boot, antes de que cualquier
    // inicialización pueda sobrescribir esa zona.
    //
    // get_panic_message_utf8() verifica un magic number de 8 bytes; si no
    // coincide (boot limpio o power-cycle), retorna None. Si hay mensaje
    // válido (viene de un soft-reset tras pánico), retorna Some(&str).
    //
    // El &str es 'static: apunta directamente a la zona PANDUMP en RAM,
    // que permanece válida durante toda la ejecución. No hay copia.
    let panic_msg: Option<&'static str> = panic_persist::get_panic_message_utf8();

    // ── PASO 2: Inicializar hardware ──────────────────────────────────────
    let p = embassy_rp::init(Default::default());
    let driver = Driver::new(p.USB, Irqs);

    // ── PASO 3: Configurar USB ─────────────────────────────────────────────
    //
    // ┌── CUSTOMIZE PER PROJECT ─────────────────────────────────────────┐
    // │ VID/PID, manufacturer y product van aquí. VID 0x2E8A/PID 0x000A  │
    // │ son los valores que Raspberry Pi reserva para una Pico con       │
    // │ USB-CDC "genérica" — picotool los reconoce sin flags extra.      │
    // │ Para un producto propio, usa tu propio VID/PID (o al menos       │
    // │ cambia manufacturer/product) para no confundirte con otra Pico.  │
    // └────────────────────────────────────────────────────────────────┘
    //
    // El serial USB reportado en modo normal debe ser el ID único de la
    // flash en hex (ver comentario junto a SERIAL_BUF) para que picotool -f
    // pueda re-encontrar el dispositivo tras el reboot a BOOTSEL. No lo
    // reemplaces por un string fijo.
    let mut flash: Flash<'_, _, Blocking, FLASH_SIZE> = Flash::new_blocking(p.FLASH);
    let mut uid = [0u8; 8];
    flash.blocking_unique_id(&mut uid).unwrap();
    let serial_bytes = SERIAL_BUF.init([0u8; 16]);
    hex_encode_upper(&uid, serial_bytes);
    let serial_str: &'static str = core::str::from_utf8(serial_bytes).unwrap();

    let mut config = Config::new(0x2E8A, 0x000A);
    config.manufacturer = Some("Raspberry Pi");
    config.product = Some(PRODUCT_NAME);
    config.serial_number = Some(serial_str);
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    // Razón del último reset. None cubre tanto power-on reset como nuestros
    // propios soft-resets (panic-persist / SCB::sys_reset()) — el RP2040 no
    // distingue esos casos en este registro, así que lo decimos tal cual.
    let reset_reason: Option<ResetReason> = Watchdog::new(p.WATCHDOG).reset_reason();

    // ADC para el sensor de temperatura interno, en modo async: la tarea se
    // suspende y el executor sigue trabajando mientras la conversión corre;
    // ADC_IRQ_FIFO la despierta al terminar.
    let adc = adc::Adc::new(p.ADC, Irqs, adc::Config::default());
    let temp_channel = adc::Channel::new_temp_sensor(p.ADC_TEMP_SENSOR);

    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESCRIPTOR.init([0; 256]),
        BOS_DESCRIPTOR.init([0; 256]),
        MSOS_DESCRIPTOR.init([0; 256]),
        CONTROL_BUF.init([0; 256]),
    );

    // ── Interfaz de reset para picotool -f ────────────────────────────────
    // Vendor class 0xFF / subclass 0x00 / protocol 0x01 — mismo que el SDK de C.
    let reset_if = {
        let mut func = builder.function(0xFF, 0x00, 0x01);
        let mut iface = func.interface();
        let _alt = iface.alt_setting(0xFF, 0x00, 0x01, None);
        iface.interface_number()
    };
    let reset_handler = RESET_HANDLER.init(PicotoolResetHandler { if_num: reset_if });
    builder.handler(reset_handler);

    let state = STATE.init(State::new());
    let class = CdcAcmClass::new(&mut builder, state, 64);
    let usb = builder.build();

    // ── PASO 4: Lanzar tareas ─────────────────────────────────────────────
    spawner.spawn(usb_task(usb).unwrap());
    spawner.spawn(serial_task(class, panic_msg).unwrap());
    spawner.spawn(app_task(uid, reset_reason, adc, temp_channel).unwrap());
}

// ─── Tarea 1: USB stack ────────────────────────────────────────────────────
#[embassy_executor::task]
async fn usb_task(mut usb: embassy_usb::UsbDevice<'static, Driver<'static, USB>>) {
    usb.run().await;
}

// ─── Tarea 2: Puerto serie CDC bidireccional ───────────────────────────────
#[embassy_executor::task]
async fn serial_task(
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
                rom_data::reset_to_usb_boot(0, 0);
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
        let _ = class.write_packet(BANNER).await;

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
                rom_data::reset_to_usb_boot(0, 0);
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
#[embassy_executor::task]
async fn app_task(
    uid: [u8; 8],
    reset_reason: Option<ResetReason>,
    mut adc: adc::Adc<'static, adc::Async>,
    mut temp_channel: adc::Channel<'static>,
) {
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
                let _ = write!(
                    resp,
                    "{} v{}\r\nFlash UID: {}\r\nUltimo reset: {}",
                    PRODUCT_NAME,
                    env!("CARGO_PKG_VERSION"),
                    hex_str,
                    reason,
                );
            }
            b"temp" => match adc.read(&mut temp_channel).await {
                Ok(raw) => {
                    let c = convert_to_celsius(raw);
                    let _ = write!(resp, "Temperatura interna: {:.1} C (raw={})", c, raw);
                }
                Err(_) => {
                    let _ = write!(resp, "Error leyendo el ADC");
                }
            },
            b"uptime" => {
                let ms = embassy_time::Instant::now().as_millis();
                let _ = write!(resp, "Uptime: {} ms", ms);
            }
            b"bootsel" => {
                let _ = write!(resp, "Reiniciando a BOOTSEL...");
                let _ = TX_CHANNEL.try_send(resp);
                Timer::after(Duration::from_millis(100)).await;
                rom_data::reset_to_usb_boot(0, 0);
                continue;
            }
            _ => {
                let _ = write!(resp, "Comando desconocido. Escribe 'help'.");
            }
        }

        let _ = TX_CHANNEL.try_send(resp);
    }
}
