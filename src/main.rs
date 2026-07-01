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
use embassy_usb::control::{OutResponse, Recipient, Request};
use embassy_usb::types::InterfaceNumber;
use embassy_usb::{Builder, Config, Handler};
use static_cell::StaticCell;

// panic-persist: en caso de pánico, escribe el mensaje en la zona PANDUMP
// (definida en memory.x) y hace un soft-reset. El USB-CDC vuelve a estar
// disponible en el siguiente boot. No uses panic-halt ni panic-reset junto
// con este crate — panic-persist ya incluye su propio #[panic_handler].
use panic_persist as _;

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
});

// ─── Canal de comunicación entre tareas ────────────────────────────────────
static RX_CHANNEL: Channel<ThreadModeRawMutex, heapless::Vec<u8, 64>, 4> = Channel::new();

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
            // disable_interface_mask=1 en el reboot pedido por picotool: deshabilita
            // la interfaz de almacenamiento masivo (RPI-RP2) en BOOTSEL, dejando solo
            // PICOBOOT. `picotool info -f` siempre manda wValue=0 (no pide esto por
            // su cuenta), así que si dejamos ambas interfaces habilitadas, el SO monta
            // el drive RPI-RP2 (udev/gvfs) y ese automount retrasa la re-enumeración
            // más allá de los ~6s que picotool espera (5 reintentos x 1.2s) antes de
            // rendirse — el reboot funciona, pero picotool ya dejó de buscar.
            // PICOBOOT es lo único que picotool necesita, así que deshabilitar MSD
            // aquí es seguro y evita la carrera.
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

// ─── Buffers estáticos para embassy-usb (StaticCell = sin unsafe) ──────────
//
// CAMBIO EN embassy-usb 0.6: Builder::new ya NO recibe device_descriptor.
// Lo genera internamente. Solo se necesitan estos 4 buffers:
//   config_descriptor, bos_descriptor, msos_descriptor, control_buf
static STATE: StaticCell<State> = StaticCell::new();
static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
static BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
static MSOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();

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
    // VID 0x2E8A = Raspberry Pi
    // PID 0x000A = Pico con USB-CDC (reconocido nativamente por picotool)
    let mut config = Config::new(0x2E8A, 0x000A);
    config.manufacturer = Some("Raspberry Pi");
    config.product = Some("Pico USB Console");
    config.serial_number = Some("ECODITEC001");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    // ── Builder con la firma correcta de embassy-usb 0.6 ──────────────────
    //
    // ERROR ANTERIOR: Builder::new recibía 7 args incluyendo device_descriptor.
    // CORRECCIÓN: en 0.6 son 6 args — device_descriptor ya no se pasa,
    // el builder lo genera internamente a partir de Config.
    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESCRIPTOR.init([0; 256]),
        BOS_DESCRIPTOR.init([0; 256]),
        MSOS_DESCRIPTOR.init([0; 256]),
        CONTROL_BUF.init([0; 64]),
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
    //
    // CAMBIO EN embassy-executor 0.10: spawner.spawn() ya no retorna Result.
    // Retorna () directamente — el .unwrap() causaba error E0599.
    // La tarea devuelve Result<SpawnToken, SpawnError>, por lo que hay que
    // usar .unwrap() en el token ANTES de pasarlo a spawn(), no después.
    //
    // Forma correcta en 0.10:
    //   spawner.spawn(mi_tarea(args).unwrap())   ← unwrap en el token
    // Forma antigua (0.6/0.7):
    //   spawner.spawn(mi_tarea(args)).unwrap()   ← unwrap en spawn()
    spawner.spawn(usb_task(usb).unwrap());
    spawner.spawn(serial_task(class, panic_msg).unwrap());
    spawner.spawn(app_task().unwrap());
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

    loop {
        // Esperar que el host abra el puerto (miniterm, screen, cat, etc.)
        class.wait_connection().await;

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
            } else {
                // Boot limpio
                let _ = class
                    .write_packet(b"[Pico USB Console - boot limpio]\r\n")
                    .await;
            }
        }

        // ── Bucle de comunicación bidireccional ────────────────────────────
        loop {
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
                    let data = &buf[..n];
                    // Eco inmediato al host
                    let _ = class.write_packet(data).await;
                    // Reenviar a app_task para procesamiento
                    let mut msg: heapless::Vec<u8, 64> = heapless::Vec::new();
                    let _ = msg.extend_from_slice(data);
                    let _ = RX_CHANNEL.try_send(msg);
                }
                Ok(Err(_)) => break, // host cerró el puerto → volver a wait_connection
                _ => {}              // Timeout o paquete de tamaño 0
            }
        }
    }
}

// ─── Tarea 3: Lógica de la aplicación ─────────────────────────────────────
// Aquí va el código real del proyecto: sensores, actuadores, protocolos, etc.
#[embassy_executor::task]
async fn app_task() {
    let mut contador: u32 = 0;

    loop {
        while let Ok(msg) = RX_CHANNEL.try_receive() {
            // Parsear comandos recibidos por USB
            // Ejemplo: if msg.starts_with(b"LED_ON") { ... }
            let _ = msg;
        }

        contador = contador.wrapping_add(1);
        // Aquí: leer ADC, I2C, SPI, controlar GPIO, etc.
        Timer::after(Duration::from_millis(500)).await;
    }
}
