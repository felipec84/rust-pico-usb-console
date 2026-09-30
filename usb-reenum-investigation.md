# Investigación: la placa no re-enumera tras `picotool load -x`

**RESUELTO el 2026-08-03.** Causa: `embassy-rp` nunca lleva el bloque USBCTRL
por RESETS, así que la app arrancaba heredando estado del controlador USB que
había dejado el bootrom. Fix: `hard_reset_usbctrl()` en `main.rs`. Verificado
en hardware, 20 transiciones BOOTSEL→app sin una sola falla (baseline: ~60%).
El detalle de cómo se llegó ahí está más abajo; se conserva completo porque
las hipótesis descartadas son la mitad del valor.

Sesión dedicada — **no mezclar con el trabajo del datalogger**, que va en
`../pico-ds18b20-datalogger`.

## Síntoma

Después de que `picotool load -f -x` reporta éxito y reinicia la placa, ésta
desaparece del bus USB **por completo**: ni la app (`2e8a:000a`) ni BOOTSEL
(`2e8a:0003`), nada en `lsusb` ni en `journalctl -k`. Esperado 60 s completos
sin recuperación. Solo vuelve desconectando y reconectando el micro-USB.

Tasa observada el 2026-08-03: **3 de 5 intentos** fallaron.

Forma exacta en el log del kernel (siempre igual):

```
usb 1-3: USB disconnect, device number N        <- reset por baud 1200
usb 1-3: new full-speed USB device number N+1
usb 1-3: Product: RP2 Boot                       <- BOOTSEL, OK
usb 1-3: USB disconnect, device number N+1       <- picotool reinicia tras cargar
(nada más — nunca reaparece)
```

Cuando funciona, la app reaparece ~1 s después de ese último disconnect.

## Hipótesis ya DESCARTADAS (no volver sobre ellas)

1. **Tick rate del watchdog mal configurado** — descartado en la sesión previa
   (`bf78628`): `embassy_rp::init()` ya configura `clk_tick` vía `clocks.rs`
   antes de que corra `main()`.

2. **El automount del disco RPI-RP2 rompe el puerto** — plausible y con
   evidencia real (el kernel registra `device offline error` y `Buffer I/O
   error on dev sda1` porque picotool reinicia en plena lectura SCSI), pero
   **NO es la causa del cuelgue**: se cambió a `disable_interface_mask=1` en
   los tres caminos de `console.rs`, se verificó que ya no aparece ningún
   `sd*`, y la placa se colgó igual. El cambio se conservó igual porque el I/O
   error es un problema real y separado (commit `92ae315` en el datalogger).

3. **El contador de bootloop de `watchdog.rs` llega a 3 y el firmware entra en
   pánico antes de levantar el USB** — teoría atractiva (explicaría "muerta
   hasta desconectar", porque `.uninit` sobrevive al soft-reset), pero
   **refutada empíricamente**: el cuelgue se reprodujo con la placa recién
   reconectada, con `info` reportando `power-on o soft-reset` — camino que pone
   el contador en CERO — y **un solo flasheo** después. Con el contador en 1 no
   puede dispararse un umbral de 3.

## Lo que sí está establecido

- Arranque en frío ⇒ `info` reporta `power-on o soft-reset`, o sea
  `reset_reason()` devuelve `None`. Confirmado.
- El firmware en sí está sano: `test/console_test.py` da **10/10** una vez que
  la placa está enumerada, incluido el ciclo real de `bootsel`.
- No hay correlación con cuánto vivió la app antes del flasheo:
  16 s → OK, 3 s → falla, 65 s → falla.
- La escritura de flash **sí se completa**: tras reconectar, la placa corre el
  firmware nuevo correctamente.

## MEDIDO el 2026-08-03 (sesión de coordinación)

### La escritura de flash NO tiene nada que ver — el bug está en BOOTSEL→app

Experimento (el nº2 de la lista de abajo), ejecutado sobre la placa con el
firmware del **datalogger**, sin cargar nada:

```sh
picotool reboot -f -u    # app -> BOOTSEL
picotool reboot          # BOOTSEL -> app
```

Resultado en 4 vueltas completas: **3 OK, 1 cuelgue** — mismo síntoma exacto
(bus vacío, ni `000a` ni `0003`, muerta hasta reconectar el micro-USB).

Conclusión: `picotool load` es un espectador inocente. El fallo es la
transición BOOTSEL→app por sí sola. Esto **elimina** toda hipótesis que
dependa de escribir flash y **refuerza** la hipótesis 3 (estado residual del
periférico USB tras el bootrom).

### Confirmado por código: nadie resetea nunca el bloque USBCTRL

En `embassy-rp` (checkout `a5387ad`), `clocks.rs:975-984`, `init()` resetea
todos los periféricos MENOS el USB, a propósito:

```rust
let mut peris = reset::ALL_PERIPHERALS;
peris.set_usbctrl(false);   // "USB, SYSCFG (breaks usb-to-swd on core1)"
reset::reset(peris);
```

Y `usb::Driver::new` no compensa: solo hace un zero-fill de los registros
`0x00..0x9C` y de los primeros `0x100` de DPRAM. Ese zero-fill **no puede
limpiar `SIE_STATUS` ni `BUFF_STATUS`, que son write-1-to-clear** —
escribirles cero los deja intactos. Viniendo de BOOTSEL, el bootrom estuvo
manejando ese mismo controlador y picotool lo reinicia en medio del tráfico,
así que la app puede arrancar con bits de estado pegados de la sesión
anterior. Encaja con todo: intermitencia, silencio total en el bus, y que
solo un power-cycle lo saque.

### Hipótesis descartada acá: carrera entre la IRQ USB y la lectura de flash

`main()` habilita la interrupción USB (en `Driver::new`) antes de llamar a
`flash.blocking_unique_id()`. Parecía una carrera candidata, pero no lo es:
`in_ram()` corre dentro de `critical_section::with` (`flash.rs:948`), así que
la IRQ no puede colarse ahí.

### El fix: `hard_reset_usbctrl()` — VERIFICADO

En `main.rs`, entre `embassy_rp::init()` (que ya configuró `clk_usb`) y
`Driver::new` (que ya empieza a escribir registros del bloque):

```rust
pac::RESETS.reset().modify(|w| w.set_usbctrl(true));
pac::RESETS.reset().modify(|w| w.set_usbctrl(false));
while !pac::RESETS.reset_done().read().usbctrl() {}
```

Es la única forma de garantizar W1C y PHY limpios. Resultados medidos:

| escenario                            | antes      | con el fix |
|--------------------------------------|------------|------------|
| `./flash.sh` ×10 (`reenum_loop.sh`)  | ~60% falla | **10/10 OK** |
| `picotool reboot` round-trip ×10     | 1 de 4 falló | **10/10 OK** |

20 transiciones sin una sola falla, todas re-enumerando en ≤1 s. Con un 60% de
falla base, 10 éxitos seguidos por azar tienen probabilidad ~1e-4.
`test/console_test.py` da 7/7 después.

### Efecto colateral encontrado y corregido: el contador de bootloop era una trampa

Pregunta abierta nº1, ahora medida: después de un `picotool load -x`, `info`
reporta **`Ultimo reset: watchdog timeout`**. Los reboots de la ROM se
implementan CON el watchdog, así que `reset_reason()` no los distingue de un
cuelgue real, y `check_bootloop()` los contaba. Consecuencia: **tres
`./flash.sh` seguidos llegaban al umbral y el firmware entraba en pánico a
propósito**, sin que hubiera ningún bug.

Corregido con el mismo esquema de pico-sdk (`watchdog_enable_caused_reboot()`):
un magic propio en `scratch[4]` del watchdog, que toda ruta de reboot de la ROM
sobrescribe. Verificado en hardware — tras un flasheo ahora `info` sigue
diciendo `watchdog timeout` (es cierto) pero `Watchdog boot count: 0`.

Esto NO era la causa del cuelgue (ya estaba refutado, y con razón), pero era un
bug real y latente que habría mordido a cualquiera flasheando tres veces
seguidas.

## Preguntas que quedaron sin responder (y ya no hacen falta)

- **¿Es específico del host/puerto?** Nunca se probó otro puerto ni otra
  máquina. Con la causa raíz identificada y corregida, dejó de importar.

## Herramientas que quedaron

- `test/console_test.py --info` — imprime la respuesta cruda de `info`.
- `test/reenum_loop.sh [N]` — N ciclos de flasheo midiendo re-enumeración,
  loguea a `test/reenum_loop.log` y reporta tasa de fallo. Si corre sin TTY
  corta limpio en la primera falla en vez de bloquearse pidiendo Enter.

## Pendiente

Nada. Los dos fixes bajaron también al datalogger (`../pico-ds18b20-datalogger`, que tenía copia de
`main.rs`/`watchdog.rs` y por eso los dos bugs): `ec484fd` y `fec60eb` en su historia, verificado el
2026-09-26 con `git -C ../pico-ds18b20-datalogger log --oneline | grep -i -E "usbctrl|bootloop"`.

## `disable_interface_mask` — HECHO y verificado

El documento decía que se había cambiado a `1` "en los tres caminos de
`console.rs`", pero en el esqueleto los tres seguían en
`reset_to_usb_boot(0, 0)`; el cambio había quedado solo en el datalogger.
Ya está aplicado acá (constante `DISABLE_MSC`).

El claim se verificó con un A/B en la misma placa, aprovechando que `main.rs`
ya usaba `1` y `console.rs` todavía usaba `0`:

- **`mask=0`** (baud 1200): el kernel engancha `usb-storage`, aparece
  `/dev/sda1` con label `RPI-RP2`, y al terminar `picotool load` el disco
  desaparece en plena operación SCSI:
  ```
  device offline error, dev sda, sector 260 op 0x1:(WRITE)
  Buffer I/O error on dev sda1, logical block 259, lost async page write
  FAT-fs (sda1): unable to read boot sector to mark fs as dirty
  ```
- **`mask=1`** (`picotool reboot -f -u`): ningún `sd*`, ni una sola línea de
  SCSI en `journalctl`, y picotool sigue funcionando igual — solo usa
  PICOBOOT, no el disco.

Verificado después en los tres puntos de llamada (baud 1200 dentro y fuera
del bucle de sesión, y el comando `bootsel`): `console_test.py
--include-bootsel` da 8/8 sin una línea de almacenamiento en el log.

Es ruido evitable en `journalctl`, **no** la causa del bug de re-enumeración
— eso ya estaba correctamente descartado en la hipótesis 2.

## Método (para la próxima cacería parecida)

Cada iteración fallida costaba una reconexión física del micro-USB, así que
Felipe tenía que estar presente. Lo que funcionó: (a) leer el código de la
dependencia en vez de suponer, (b) gastar la primera reconexión en el
experimento que más hipótesis separaba — reboot sin escribir flash —, y (c)
notar que si el fix sirve, las iteraciones exitosas son gratis, así que probar
un fix candidato es mucho más barato que seguir caracterizando el fallo.

Workaround histórico, ya innecesario: flashear con el **botón BOOTSEL físico**
evita por completo el camino app→BOOTSEL→app.
