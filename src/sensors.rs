// ─── Sensores: dueño exclusivo del ADC ─────────────────────────────────────
// Patrón: un módulo posee el/los periférico(s), corre un loop de muestreo en
// su propia tarea, y publica el último valor en un estado compartido
// protegido por mutex. Quien consume (la consola, otro módulo) solo llama
// `get_status()` — nunca bloquea, nunca toca el ADC directamente.
//
// ┌── CUSTOMIZE PER PROJECT ─────────────────────────────────────────────────┐
// │ El sensor de temperatura interno del RP2040 es el único de fábrica; es   │
// │ un placeholder para demostrar el patrón. Para agregar un sensor propio   │
// │ (I2C, 1-Wire, otro canal ADC...):                                        │
// │  1. Súmale el/los pines a `SensorsResources` en resources.rs.            │
// │  2. Agrega su campo a `SensorState` y su lectura al loop de abajo.       │
// │ Si el sensor tiene una conversión lenta (1-Wire ronda los 750 ms) NO la  │
// │ hagas dentro de un handler de comando en console.rs — hazla acá, en el   │
// │ loop de esta tarea, y deja que la consola solo lea el último valor       │
// │ cacheado con get_status().                                               │
// └───────────────────────────────────────────────────────────────────────────┘

use core::cell::RefCell;
use embassy_rp::adc::{self, Adc, Channel};
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_time::{Duration, Timer};

use crate::resources::SensorsResources;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SensorState {
    pub temperature_c: f32, // Temperatura interna, filtrada (EMA)
    pub raw_temp: u16,      // Valor crudo del ADC (12-bit, 0-4095)
}

static STATUS: BlockingMutex<ThreadModeRawMutex, RefCell<SensorState>> =
    BlockingMutex::new(RefCell::new(SensorState {
        temperature_c: 0.0,
        raw_temp: 0,
    }));

/// Lee el último valor muestreado. No bloquea, no toca el ADC.
pub fn get_status() -> SensorState {
    STATUS.lock(|cell| *cell.borrow())
}

fn update_status(temperature_c: f32, raw_temp: u16) {
    STATUS.lock(|cell| {
        let mut s = cell.borrow_mut();
        s.temperature_c = temperature_c;
        s.raw_temp = raw_temp;
    });
}

// Fórmula de calibración del sensor de temperatura interno (RP2040 datasheet §4.9.5).
fn convert_to_celsius(raw_temp: u16) -> f32 {
    let temp = 27.0 - (raw_temp as f32 * 3.3 / 4096.0 - 0.706) / 0.001721;
    let sign = if temp < 0.0 { -1.0 } else { 1.0 };
    let rounded_temp_x10: i16 = ((temp * 10.0) + 0.5 * sign) as i16;
    (rounded_temp_x10 as f32) / 10.0
}

#[embassy_executor::task]
pub async fn sensors_task(r: SensorsResources) {
    // ADC en modo async: la tarea se suspende y el executor sigue trabajando
    // mientras la conversión corre; ADC_IRQ_FIFO la despierta al terminar.
    let mut adc = Adc::new(r.adc, crate::Irqs, adc::Config::default());
    let mut temp_channel = Channel::new_temp_sensor(r.temp_sensor);

    // Suavizado EMA (10%): sensores ADC en placas prototipo son ruidosos por
    // diseño (ver datasheet, ripple de alimentación por USB); sin filtro cada
    // lectura salta varios LSB entre muestras.
    let alpha: f32 = 0.1;
    let mut ema_temp_c: f32 = 25.0; // valor inicial razonable, no 0.0

    loop {
        let raw_temp: u16 = adc.read(&mut temp_channel).await.unwrap_or_default();
        let raw_temp_c = convert_to_celsius(raw_temp);
        ema_temp_c = (alpha * raw_temp_c) + ((1.0 - alpha) * ema_temp_c);

        update_status(ema_temp_c, raw_temp);

        Timer::after(Duration::from_millis(100)).await;
    }
}
