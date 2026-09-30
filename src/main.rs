#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::adc; // qualificado a propósito: adc::InterruptHandler
// colisiona de nombre con embassy_rp::usb::InterruptHandler.
use embassy_rp::bind_interrupts;
use embassy_rp::flash::{Blocking, Flash};
use embassy_rp::pac;
use embassy_rp::peripherals::USB;
use embassy_rp::rom_data;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_rp::watchdog::{ResetReason, Watchdog};
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

// resources: reparto de periféricos por subsistema vía `assign-resources`
// (assign_resources!/split_resources!). Ver resources.rs.
mod resources;
// Los structs de recursos (UsbConsoleResources, SensorsResources, ...)
// generados por assign_resources! en resources.rs deben quedar en scope acá
// porque split_resources!() los nombra sin calificar. `split_resources!` en
// sí queda definida en la RAÍZ del crate (assign_resources! la exporta con
// #[macro_export] sin importar en qué módulo se invocó), así que se usa
// directo, sin `use`.
use resources::*;

// console: la consola USB-CDC (tareas usb_task/serial_task/app_task, los
// canales entre ellas y la lógica de comandos). main() solo arma el
// hardware y las lanza — ver console.rs para la sustancia.
mod console;

// sensors: dueño exclusivo del ADC. Patrón a seguir para cualquier sensor
// propio — ver el comentario de cabecera en sensors.rs.
mod sensors;

// watchdog: feed periódico del watchdog + protección contra bootloop.
mod watchdog;

// ─── Identidad del producto ────────────────────────────────────────────────
// CUSTOMIZE PER PROJECT: nombre visible en lsusb/picotool y en el banner.
// Los asserts se evalúan EN COMPILACIÓN — un nombre demasiado largo aquí no
// compila, en vez de hacer que embassy-usb entre en pánico serializando el
// string descriptor durante la enumeración (síntoma: la Pico se resetea en
// bucle y el host registra "can't set config #1, error -32").
pub(crate) const PRODUCT_NAME: &str = "{{product-name}}";
pub(crate) const BANNER: &[u8] = b"[{{product-name}} - escribe 'help']\r\n";

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
    embassy_rp::binary_info::rp_program_name!(c"{{product-name}}"),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_description!(c"USB CDC Console with Auto-Reset"),
];

// ─── Interrupciones ────────────────────────────────────────────────────────
// pub(crate): console.rs necesita referenciar este mismo `Irqs` (como
// `crate::Irqs`) para inicializar el ADC — ver main() más abajo.
bind_interrupts!(pub(crate) struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

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

// ─── Reset duro del bloque USBCTRL ─────────────────────────────────────────
//
// `embassy_rp::init()` resetea casi todos los periféricos, pero deja el USB
// FUERA a propósito (clocks.rs: `peris.set_usbctrl(false)`, con el comentario
// "USB, SYSCFG (breaks usb-to-swd on core1)"). Y `usb::Driver::new` NO
// compensa eso con un reset: solo hace un zero-fill de los registros
// 0x00..0x9C y de los primeros 0x100 de la DPRAM.
//
// Ese zero-fill es insuficiente porque SIE_STATUS y BUFF_STATUS son
// write-1-to-clear: escribirles cero NO los limpia. Cuando venimos de
// BOOTSEL, el bootrom estuvo manejando este mismo controlador USB y picotool
// lo reinicia en medio del tráfico, así que la app puede arrancar con bits
// de estado pegados de la sesión anterior — el síntoma es que la placa nunca
// se presenta al host y queda muerta hasta desconectar el USB.
//
// Llevar el bloque por RESETS es la única forma de garantizar un estado
// inicial limpio: el reset de periférico sí borra los W1C y el PHY.
//
// Debe llamarse DESPUÉS de `embassy_rp::init()` (que configura clk_usb) y
// ANTES de `Driver::new`, que ya empieza a escribir registros del bloque.
fn hard_reset_usbctrl() {
    pac::RESETS.reset().modify(|w| w.set_usbctrl(true));
    pac::RESETS.reset().modify(|w| w.set_usbctrl(false));
    // reset_done se levanta cuando el bloque terminó de salir del reset;
    // tocar sus registros antes de eso es escribir al vacío.
    while !pac::RESETS.reset_done().read().usbctrl() {}
}

// Tamaño físico de la flash (Winbond/QSPI en la Pico), no el tamaño usado
// por el linker en memory.x — necesario para el driver Flash de embassy-rp.
const FLASH_SIZE: usize = 2 * 1024 * 1024;

// picotool identifica, en modo BOOTSEL, al RP2040 por su ID único de flash
// (picoboot_connection.c: para RP2040 compara el flash ID vía PICOBOOT, NO
// el string de serie USB). Para que `picotool -f` pueda re-encontrar el
// dispositivo tras el reboot, el serial USB en modo normal debe ser ese
// mismo ID en hex (igual que hace pico-sdk con pico_get_unique_board_id()).
// Un serial arbitrario como "MY-DEVICE-01" hace que picotool nunca reconozca
// el dispositivo reiniciado y agote sus reintentos, aunque el reboot en sí
// funcione.
static SERIAL_BUF: StaticCell<[u8; 16]> = StaticCell::new();

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

    // Tres arranques seguidos desde un pánico ⇒ BOOTSEL, para que un pánico
    // anterior al USB no deje la placa inalcanzable. Ver watchdog.rs.
    watchdog::check_panic_loop(panic_msg.is_some());

    // ── PASO 2: Inicializar hardware ──────────────────────────────────────
    let p = embassy_rp::init(Default::default());

    // Reparto de periféricos por subsistema (ver resources.rs): a partir de
    // acá ya no se toca `p.XXX` directamente, todo pasa por los grupos de
    // `r` (r.usb_console, r.sensors, ...).
    let r = split_resources!(p);

    // Estado limpio del controlador USB antes de tocarlo — ver la nota junto
    // a hard_reset_usbctrl(). Sin esto, el arranque después de un
    // `picotool load -x` puede heredar bits pegados del bootrom.
    hard_reset_usbctrl();

    let driver = Driver::new(r.usb_console.usb, Irqs);

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
    let mut flash: Flash<'_, _, Blocking, FLASH_SIZE> = Flash::new_blocking(r.usb_console.flash);
    let mut uid = [0u8; 8];
    flash.blocking_unique_id(&mut uid).unwrap();
    let serial_bytes = SERIAL_BUF.init([0u8; 16]);
    console::hex_encode_upper(&uid, serial_bytes);
    let serial_str: &'static str = core::str::from_utf8(serial_bytes).unwrap();

    let mut config = Config::new(0x2E8A, 0x000A);
    config.manufacturer = Some("{{manufacturer}}");
    config.product = Some(PRODUCT_NAME);
    config.serial_number = Some(serial_str);
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    // Razón del último reset. None cubre tanto power-on reset como nuestros
    // propios soft-resets (panic-persist / SCB::sys_reset()) — el RP2040 no
    // distingue esos casos en este registro, así que lo decimos tal cual.
    let mut watchdog = Watchdog::new(r.usb_console.watchdog);
    let reset_reason: Option<ResetReason> = watchdog.reset_reason();

    // Debe llamarse antes de spawnear ninguna tarea (ver watchdog.rs):
    // corta el arranque con panic!() si venimos de >= 3 reinicios seguidos
    // por timeout del watchdog, en vez de seguir reintentando para siempre.
    // Necesita el watchdog prestado para leer/re-armar el magic de scratch[4]
    // con que distingue un cuelgue real de un reboot pedido por picotool.
    watchdog::check_bootloop(&mut watchdog, reset_reason);

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

    // Solo para probar check_panic_loop en hardware: un pánico en el mismo
    // punto que el assert! de max-interface-count, antes de que el USB se
    // presente al host.
    #[cfg(feature = "test-panic-before-usb")]
    panic!("prueba: panico forzado antes del USB (feature test-panic-before-usb)");

    // ── PASO 4: Lanzar tareas ─────────────────────────────────────────────
    spawner.spawn(console::usb_task(usb).unwrap());
    spawner.spawn(console::serial_task(class, panic_msg).unwrap());
    spawner.spawn(console::app_task(uid, reset_reason).unwrap());
    spawner.spawn(sensors::sensors_task(r.sensors).unwrap());
    spawner.spawn(watchdog::watchdog_task(watchdog).unwrap());
}
