// ─── Reparto de periféricos por subsistema ─────────────────────────────────
// `embassy_rp::init()` devuelve un único `Peripherals` que es dueño de TODOS
// los pines y periféricos del chip. En vez de ir sacando campos sueltos
// (`p.USB`, `p.ADC`, `p.PIN_16`, ...) a mano dentro de `main()` —lo que obliga
// a que `main` conozca el pinout de cada subsistema—, el macro
// `assign_resources!` parte `Peripherals` en grupos con nombre, uno por
// módulo.
//
// Cómo se usa (ver main.rs):
//
//     let p = embassy_rp::init(Default::default());
//     let r = split_resources!(p);
//     spawner.spawn(sensors::sensors_task(r.sensors).unwrap());
//
// Cada campo de `r` es el struct del grupo (p. ej. `SensorsResources`), y cada
// campo DENTRO de ese struct es un `Peri<'static, peripherals::XXX>` — el
// mismo tipo que tendría `p.XXX`, solo que movido a un struct con nombre.
//
// ┌── CUSTOMIZE PER PROJECT ────────────────────────────────────────────────┐
// │ Agrega acá un grupo por cada subsistema nuevo, con los pines que ese    │
// │ módulo va a poseer en exclusiva. Que el reparto esté declarado en un    │
// │ solo archivo es lo que hace que un conflicto de pines sea evidente a    │
// │ simple vista — y, si asignas el mismo pin dos veces, no compila.        │
// └─────────────────────────────────────────────────────────────────────────┘

use assign_resources::assign_resources;
use embassy_rp::Peri;
use embassy_rp::peripherals;

assign_resources! {
    // Consola USB-CDC (console.rs) + identidad y supervisión del equipo:
    // el periférico USB, la flash (de donde sale el ID único que se reporta
    // como número de serie USB — ver main.rs) y el watchdog (watchdog.rs).
    usb_console: UsbConsoleResources {
        usb: USB,
        flash: FLASH,
        watchdog: WATCHDOG,
    }

    // Sensores analógicos (sensors.rs): el ADC y el sensor de temperatura
    // interno del RP2040. Para agregar entradas analógicas propias, súmalas
    // acá (p. ej. `presion: PIN_26`) y créales un `adc::Channel` en la tarea.
    sensors: SensorsResources {
        adc: ADC,
        temp_sensor: ADC_TEMP_SENSOR,
    }
}
